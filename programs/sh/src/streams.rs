use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, ready};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::future::LocalBoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::Stream;

#[derive(Clone)]
pub struct InputStream {
    inner: Rc<RefCell<Pin<Box<dyn AsyncRead>>>>,
}

#[derive(Clone)]
pub struct OutputStream {
    inner: Rc<RefCell<Pin<Box<dyn AsyncWrite>>>>,
}

#[derive(Clone)]
pub struct CapturedOutput {
    bytes: Rc<RefCell<Vec<u8>>>,
}

impl InputStream {
    pub fn new(reader: impl AsyncRead + 'static) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Box::pin(reader))),
        }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::new(BytesReader { bytes, cursor: 0 })
    }

    pub fn empty() -> Self {
        Self::from_bytes(Vec::new())
    }

    pub fn closed(fd: i32) -> Self {
        Self::new(ClosedReader { fd })
    }
}

impl OutputStream {
    pub fn new(writer: impl AsyncWrite + 'static) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Box::pin(writer))),
        }
    }

    pub fn closed(fd: i32) -> Self {
        Self::new(ClosedWriter { fd })
    }
}

pub fn capture_output() -> (OutputStream, CapturedOutput) {
    let bytes = Rc::new(RefCell::new(Vec::new()));
    (
        OutputStream::new(CaptureWriter {
            bytes: bytes.clone(),
        }),
        CapturedOutput { bytes },
    )
}

impl CapturedOutput {
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.bytes.borrow_mut())
    }
}

impl AsyncRead for InputStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.borrow_mut().as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for OutputStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.borrow_mut().as_mut().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.borrow_mut().as_mut().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.borrow_mut().as_mut().poll_close(cx)
    }
}

pub fn shared_buffer_file(
    initial: Vec<u8>,
    persist: impl Fn(Vec<u8>) -> LocalBoxFuture<'static, io::Result<()>> + 'static,
) -> (InputStream, OutputStream) {
    let state = Rc::new(RefCell::new(SharedBufferState {
        bytes: initial,
        cursor: 0,
        dirty: false,
    }));
    let persist = Rc::new(persist);
    (
        InputStream::new(SharedBufferReader {
            state: state.clone(),
        }),
        OutputStream::new(SharedBufferWriter {
            state,
            persist,
            pending: None,
        }),
    )
}

pub fn pipe() -> (InputStream, OutputStream) {
    let (sender, receiver) = unbounded();
    (
        InputStream::new(PipeReader::new(receiver)),
        OutputStream::new(PipeWriter::new(sender)),
    )
}

pub async fn read_all(reader: &mut InputStream) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = poll_fn(|cx| Pin::new(&mut *reader).poll_read(cx, &mut buffer)).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

pub async fn write_all(writer: &mut OutputStream, mut bytes: &[u8]) -> io::Result<()> {
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

pub async fn close(writer: &mut OutputStream) -> io::Result<()> {
    poll_fn(|cx| Pin::new(&mut *writer).poll_close(cx)).await
}

struct BytesReader {
    bytes: Vec<u8>,
    cursor: usize,
}

impl AsyncRead for BytesReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.cursor >= self.bytes.len() {
            return Poll::Ready(Ok(0));
        }

        let remaining = self.bytes.len() - self.cursor;
        let count = remaining.min(buf.len());
        buf[..count].copy_from_slice(&self.bytes[self.cursor..self.cursor + count]);
        self.cursor += count;
        Poll::Ready(Ok(count))
    }
}

struct PipeReader {
    receiver: UnboundedReceiver<Vec<u8>>,
    pending: Option<Vec<u8>>,
    cursor: usize,
}

impl PipeReader {
    fn new(receiver: UnboundedReceiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            pending: None,
            cursor: 0,
        }
    }
}

impl AsyncRead for PipeReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            if self.pending.is_some() {
                let cursor = self.cursor;
                let (count, consumed_all) = {
                    let chunk = self.pending.as_ref().expect("pending chunk must exist");
                    if cursor >= chunk.len() {
                        (0, true)
                    } else {
                        let available = chunk.len() - cursor;
                        let count = available.min(buf.len());
                        buf[..count].copy_from_slice(&chunk[cursor..cursor + count]);
                        (count, cursor + count == chunk.len())
                    }
                };

                if count > 0 {
                    self.cursor += count;
                    if consumed_all {
                        self.pending = None;
                        self.cursor = 0;
                    }
                    return Poll::Ready(Ok(count));
                }

                if consumed_all {
                    self.pending = None;
                    self.cursor = 0;
                    continue;
                }
            }

            match ready!(Pin::new(&mut self.receiver).poll_next(cx)) {
                Some(chunk) => {
                    self.pending = Some(chunk);
                }
                None => return Poll::Ready(Ok(0)),
            }
        }
    }
}

struct PipeWriter {
    sender: Option<UnboundedSender<Vec<u8>>>,
}

impl PipeWriter {
    fn new(sender: UnboundedSender<Vec<u8>>) -> Self {
        Self {
            sender: Some(sender),
        }
    }
}

impl AsyncWrite for PipeWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let Some(sender) = self.sender.as_mut() else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pipe writer is closed",
            )));
        };

        sender
            .unbounded_send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pipe reader closed"))?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.sender = None;
        Poll::Ready(Ok(()))
    }
}

struct CaptureWriter {
    bytes: Rc<RefCell<Vec<u8>>>,
}

impl AsyncWrite for CaptureWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes.borrow_mut().extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct ClosedReader {
    fd: i32,
}

impl AsyncRead for ClosedReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other(format!(
            "file descriptor {} is closed",
            self.fd
        ))))
    }
}

struct ClosedWriter {
    fd: i32,
}

impl AsyncWrite for ClosedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other(format!(
            "file descriptor {} is closed",
            self.fd
        ))))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other(format!(
            "file descriptor {} is closed",
            self.fd
        ))))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct SharedBufferState {
    bytes: Vec<u8>,
    cursor: usize,
    dirty: bool,
}

struct SharedBufferReader {
    state: Rc<RefCell<SharedBufferState>>,
}

impl AsyncRead for SharedBufferReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.state.borrow_mut();
        if state.cursor >= state.bytes.len() {
            return Poll::Ready(Ok(0));
        }

        let available = state.bytes.len() - state.cursor;
        let count = available.min(buf.len());
        buf[..count].copy_from_slice(&state.bytes[state.cursor..state.cursor + count]);
        state.cursor += count;
        Poll::Ready(Ok(count))
    }
}

struct SharedBufferWriter {
    state: Rc<RefCell<SharedBufferState>>,
    persist: Rc<dyn Fn(Vec<u8>) -> LocalBoxFuture<'static, io::Result<()>>>,
    pending: Option<LocalBoxFuture<'static, io::Result<()>>>,
}

impl SharedBufferWriter {
    fn schedule_persist(&mut self) {
        if self.pending.is_some() {
            return;
        }

        let bytes = {
            let mut state = self.state.borrow_mut();
            if !state.dirty {
                return;
            }
            state.dirty = false;
            state.bytes.clone()
        };
        self.pending = Some((self.persist)(bytes));
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.schedule_persist();
        let Some(pending) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };

        match pending.as_mut().poll(cx) {
            Poll::Ready(result) => {
                if result.is_err() {
                    self.state.borrow_mut().dirty = true;
                }
                self.pending = None;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for SharedBufferWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.state.borrow_mut();
        let start = state.cursor;
        let end = start + buf.len();
        if end > state.bytes.len() {
            state.bytes.resize(end, 0);
        }
        state.bytes[start..end].copy_from_slice(buf);
        state.cursor = end;
        state.dirty = true;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_pending(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_pending(cx)
    }
}

async fn poll_fn<T>(mut f: impl FnMut(&mut Context<'_>) -> Poll<T>) -> T {
    futures::future::poll_fn(move |cx| f(cx)).await
}
