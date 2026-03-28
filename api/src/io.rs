//! Async I/O primitives modeled after `async_std::io`.
//!
//! `stdin()` returns a stateful reader, so it is consumed through
//! `AsyncRead`/`ReadExt` and therefore uses `&mut self`.
//! `stdout()` and `stderr()` are lightweight handles: whole-buffer writes can
//! use `&self`, while streaming writes go through a dedicated writer that
//! implements `AsyncWrite`.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::vec::Vec;

use crate::bindings::wasi::cli::{stderr, stdin, stdout};
use crate::error;
use crate::error::Result;
use futures_io::{AsyncRead, AsyncWrite};
use futures_lite::future::{poll_fn, zip};
pub use futures_lite::{AsyncReadExt as ReadExt, AsyncWriteExt as WriteExt};

type IoFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + 'static>>;

/// Returns the process standard input handle.
pub fn stdin() -> Stdin {
    Stdin::new()
}

/// Returns the process standard output handle.
pub fn stdout() -> Stdout {
    Stdout::new()
}

/// Returns the process standard error handle.
pub fn stderr() -> Stderr {
    Stderr::new()
}

/// Copies all bytes from `reader` into `writer`.
pub async fn copy<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut total = 0_u64;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = poll_fn(|cx| Pin::new(&mut *reader).poll_read(cx, &mut buffer)).await?;
        if read == 0 {
            break;
        }

        write_all_poll(writer, &buffer[..read]).await?;
        total += read as u64;
    }

    poll_fn(|cx| Pin::new(&mut *writer).poll_flush(cx)).await?;
    Ok(total)
}

/// Stateful stdin reader.
#[derive(Default)]
pub struct Stdin {
    buffer: Vec<u8>,
    cursor: usize,
    loaded: bool,
    pending: Option<IoFuture<Vec<u8>>>,
}

/// Lightweight stdout handle for whole-buffer writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stdout;

/// Lightweight stderr handle for whole-buffer writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stderr;

/// Streaming stdout writer that implements `AsyncWrite`.
pub struct StdoutWriter {
    inner: OutputBuffer,
}

/// Streaming stderr writer that implements `AsyncWrite`.
pub struct StderrWriter {
    inner: OutputBuffer,
}

impl Stdin {
    fn new() -> Self {
        Self::default()
    }
}

impl Stdout {
    fn new() -> Self {
        Self
    }

    /// Returns a dedicated streaming writer for stdout.
    pub fn writer(&self) -> StdoutWriter {
        StdoutWriter {
            inner: OutputBuffer::new(OutputSink::Stdout),
        }
    }

    /// Writes a complete buffer to stdout in one operation.
    pub async fn write_all(&self, bytes: impl AsRef<[u8]>) -> Result<()> {
        write_with_sink_owned(bytes.as_ref().to_vec(), OutputSink::Stdout).await
    }

    /// Writes UTF-8 text to stdout in one operation.
    pub async fn write_str(&self, text: &str) -> Result<()> {
        self.write_all(text.as_bytes()).await
    }

    /// Writes UTF-8 text followed by `\n` to stdout in one operation.
    pub async fn writeln(&self, text: &str) -> Result<()> {
        self.write_all(line_bytes(text)).await
    }
}

impl Stderr {
    fn new() -> Self {
        Self
    }

    /// Returns a dedicated streaming writer for stderr.
    pub fn writer(&self) -> StderrWriter {
        StderrWriter {
            inner: OutputBuffer::new(OutputSink::Stderr),
        }
    }

    /// Writes a complete buffer to stderr in one operation.
    pub async fn write_all(&self, bytes: impl AsRef<[u8]>) -> Result<()> {
        write_with_sink_owned(bytes.as_ref().to_vec(), OutputSink::Stderr).await
    }

    /// Writes UTF-8 text to stderr in one operation.
    pub async fn write_str(&self, text: &str) -> Result<()> {
        self.write_all(text.as_bytes()).await
    }

    /// Writes UTF-8 text followed by `\n` to stderr in one operation.
    pub async fn writeln(&self, text: &str) -> Result<()> {
        self.write_all(line_bytes(text)).await
    }
}

impl AsyncRead for Stdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        loop {
            if self.cursor < self.buffer.len() {
                let available = self.buffer.len() - self.cursor;
                let count = available.min(buf.len());
                buf[..count].copy_from_slice(&self.buffer[self.cursor..self.cursor + count]);
                self.cursor += count;
                return Poll::Ready(Ok(count));
            }

            if self.loaded {
                return Poll::Ready(Ok(0));
            }

            if self.pending.is_none() {
                self.pending = Some(Box::pin(read_stdin_all()));
            }

            let future = self.pending.as_mut().expect("stdin pending future missing");
            match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(bytes)) => {
                    self.buffer = bytes;
                    self.cursor = 0;
                    self.loaded = true;
                    self.pending = None;
                }
                Poll::Ready(Err(error)) => {
                    self.pending = None;
                    self.loaded = true;
                    return Poll::Ready(Err(error));
                }
            }
        }
    }
}

impl AsyncWrite for StdoutWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_close(cx)
    }
}

impl AsyncWrite for StderrWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_close(cx)
    }
}

async fn read_stdin_all() -> Result<Vec<u8>> {
    let (stream, result) = stdin::read_via_stream();
    let bytes = stream.collect().await;
    result.await.map_err(error::stdin)?;
    Ok(bytes)
}

async fn write_with_sink_owned(bytes: Vec<u8>, sink: OutputSink) -> Result<()> {
    let (mut tx, rx) = crate::bindings::wit_stream::new();
    let (write_result, feed_result) = zip(
        async move {
            match sink {
                OutputSink::Stdout => stdout::write_via_stream(rx).await.map_err(error::stdout),
                OutputSink::Stderr => stderr::write_via_stream(rx).await.map_err(error::stderr),
            }
        },
        async {
            tx.write(bytes).await;
            drop(tx);
            Ok::<(), std::io::Error>(())
        },
    )
    .await;
    feed_result?;
    write_result
}

async fn write_all_poll<W>(writer: &mut W, mut bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    while !bytes.is_empty() {
        let written = poll_fn(|cx| Pin::new(&mut *writer).poll_write(cx, bytes)).await?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "async writer returned zero bytes",
            ));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

struct OutputBuffer {
    sink: OutputSink,
    buffer: Vec<u8>,
    pending: Option<IoFuture<()>>,
}

impl OutputBuffer {
    fn new(sink: OutputSink) -> Self {
        Self {
            sink,
            buffer: Vec::new(),
            pending: None,
        }
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        self.buffer.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        if self.buffer.is_empty() {
            return Poll::Ready(Ok(()));
        }

        let bytes = std::mem::take(&mut self.buffer);
        self.pending = Some(Box::pin(write_with_sink_owned(bytes, self.sink)));
        self.poll_pending(cx)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(future) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };

        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                Poll::Ready(result)
            }
        }
    }
}

fn line_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(b'\n');
    bytes
}

#[derive(Clone, Copy)]
enum OutputSink {
    Stdout,
    Stderr,
}
