//! `http-kit-io` over `wasi:sockets` and `wasi:clocks`.
//!
//! [`HeliosNet`] is the implementation of the capability traits an HTTP
//! client asks of its platform: [`Dial`] opens TCP connections, [`Resolve`]
//! looks names up, and [`Timer`] waits on the monotonic clock.
//! [`HeliosExecutor`] runs the client's local tasks on the component's
//! executor.
//!
//! Everything is a thin mapping onto the interfaces the kernel serves to a
//! component: nothing here buffers, retries or times out on its own, because
//! the client above it owns those policies. A connection reads through the
//! stream `tcp-socket.receive` returns and writes through the one handed to
//! `tcp-socket.send`; closing the write side is dropping that stream, which
//! `wasi:sockets` defines as `shutdown(SHUT_WR)`.

use std::borrow::Borrow;
use std::fs::File;
use std::future::Future;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(unix)]
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use executor_core::LocalExecutor;
use executor_core::async_task::{AsyncTask, spawn_local};
use futures_lite::stream::{Stream, once_future};
#[cfg(unix)]
use http_kit_io::DialUnix;
#[cfg(any(target_os = "linux", target_os = "android"))]
use http_kit_io::splice::{FilledPipe, Pipe, Spliced};
use http_kit_io::{
    Answer, BufResult, Dial, IoBuf, IoBufMut, IoVectoredBuf, Read, ReadAhead, ReadOwned, Received,
    Resolve, SendFile, SpliceRead, SpliceWrite, Split, TcpOptions, Timer, Write,
};
use thiserror::Error;
use wit_bindgen::rt::async_support::{FutureReader, StreamReader, StreamWriter};

use crate::bindings::wasi::clocks::monotonic_clock;
use crate::bindings::wasi::sockets::ip_name_lookup::{self, ErrorCode as LookupCode};
use crate::bindings::wasi::sockets::types::{
    ErrorCode as SocketCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress,
    Ipv6SocketAddress, TcpSocket,
};
use crate::bindings::wit_stream;
use crate::stream_closed;

/// Bytes one read asks the receive stream for.
///
/// It bounds the buffer a read lends to its caller; a larger read is served
/// by several, as a socket would.
const RECEIVE_CHUNK_BYTES: usize = 64 * 1024;

/// A `wasi:sockets` failure, kept as the source of the [`io::Error`] it
/// became so a caller can still ask which code the host answered.
#[derive(Debug, Error)]
pub enum SocketError {
    /// A `wasi:sockets/types` operation failed.
    #[error("wasi:sockets: {0:?}")]
    Socket(SocketCode),
    /// A `wasi:sockets/ip-name-lookup` operation failed.
    #[error("wasi:sockets name lookup: {0:?}")]
    Lookup(LookupCode),
}

/// The [`io::ErrorKind`] each `wasi:sockets/types` error code stands for.
///
/// Callers above this layer decide on the kind alone: `remote-unreachable` is
/// a host or network that cannot be reached, `access-denied` is a refusal by
/// policy, and `timeout` is the host's own deadline.
fn socket_error(code: SocketCode) -> io::Error {
    let kind = match &code {
        SocketCode::AccessDenied => io::ErrorKind::PermissionDenied,
        SocketCode::NotSupported => io::ErrorKind::Unsupported,
        SocketCode::InvalidArgument => io::ErrorKind::InvalidInput,
        SocketCode::OutOfMemory => io::ErrorKind::OutOfMemory,
        SocketCode::Timeout => io::ErrorKind::TimedOut,
        SocketCode::InvalidState => io::ErrorKind::InvalidInput,
        SocketCode::AddressNotBindable => io::ErrorKind::AddrNotAvailable,
        SocketCode::AddressInUse => io::ErrorKind::AddrInUse,
        SocketCode::RemoteUnreachable => io::ErrorKind::HostUnreachable,
        SocketCode::ConnectionRefused => io::ErrorKind::ConnectionRefused,
        SocketCode::ConnectionBroken => io::ErrorKind::BrokenPipe,
        SocketCode::ConnectionReset => io::ErrorKind::ConnectionReset,
        SocketCode::ConnectionAborted => io::ErrorKind::ConnectionAborted,
        SocketCode::DatagramTooLarge => io::ErrorKind::InvalidInput,
        SocketCode::Other(_) => io::ErrorKind::Other,
    };
    io::Error::new(kind, SocketError::Socket(code))
}

/// The [`io::ErrorKind`] each `wasi:sockets/ip-name-lookup` error code stands
/// for. A name that does not exist is [`io::ErrorKind::NotFound`], as
/// [`Resolve`] requires.
fn lookup_error(code: LookupCode) -> io::Error {
    let kind = match &code {
        LookupCode::AccessDenied => io::ErrorKind::PermissionDenied,
        LookupCode::InvalidArgument => io::ErrorKind::InvalidInput,
        LookupCode::NameUnresolvable => io::ErrorKind::NotFound,
        LookupCode::TemporaryResolverFailure
        | LookupCode::PermanentResolverFailure
        | LookupCode::Other(_) => io::ErrorKind::Other,
    };
    io::Error::new(kind, SocketError::Lookup(code))
}

/// The `http-kit-io` implementation of a Helios component: a zero-sized
/// handle, free to copy, over the host's `wasi:sockets` and monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeliosNet;

impl Dial for HeliosNet {
    type Connection = TcpConnection;

    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpConnection> {
        let family = match addr {
            SocketAddr::V4(_) => IpAddressFamily::Ipv4,
            SocketAddr::V6(_) => IpAddressFamily::Ipv6,
        };
        let socket = TcpSocket::create(family).map_err(socket_error)?;
        socket
            .connect(to_wasi_address(addr))
            .await
            .map_err(socket_error)?;
        let (receive, received) = socket.receive();
        let (sender, outgoing) = wit_stream::new::<u8>();
        let sent = socket.send(outgoing);
        let socket = Rc::new(socket);
        Ok(TcpConnection {
            reader: ReadHalf {
                stream: receive,
                outcome: Some(received),
                _socket: Rc::clone(&socket),
            },
            writer: WriteHalf {
                stream: Some(sender),
                outcome: Some(sent),
                _socket: socket,
            },
        })
    }
}

/// `wasi:sockets` has no Unix-domain sockets, so every dial is refused as
/// [`io::ErrorKind::Unsupported`]. The trait is asked for only where the
/// crate is built for a Unix host.
#[cfg(unix)]
impl DialUnix for HeliosNet {
    type UnixConnection = TcpConnection;

    async fn connect_unix(&self, _path: &Path) -> io::Result<TcpConnection> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "wasi:sockets has no Unix-domain sockets",
        ))
    }
}

impl Resolve for HeliosNet {
    /// One [`Answer::Ranked`]: `resolve-addresses` has no family parameter,
    /// and the host's answer is the addresses in the order it would connect
    /// to them.
    fn lookup(&self, host: &str) -> impl Stream<Item = Answer> {
        let host = host.to_owned();
        once_future(async move {
            Answer::Ranked(
                ip_name_lookup::resolve_addresses(host)
                    .await
                    .map(|addresses| addresses.into_iter().map(from_wasi_ip).collect())
                    .map_err(lookup_error),
            )
        })
    }
}

impl Timer for HeliosNet {
    async fn sleep_until(&self, deadline: Instant) {
        let wait = deadline.saturating_duration_since(Instant::now());
        // A wait longer than `u64` nanoseconds (584 years) is never reached.
        let nanos = u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX);
        monotonic_clock::wait_for(nanos).await;
    }
}

fn to_wasi_address(addr: SocketAddr) -> IpSocketAddress {
    match addr {
        SocketAddr::V4(addr) => IpSocketAddress::Ipv4(Ipv4SocketAddress {
            port: addr.port(),
            address: addr.ip().octets().into(),
        }),
        SocketAddr::V6(addr) => IpSocketAddress::Ipv6(Ipv6SocketAddress {
            port: addr.port(),
            flow_info: addr.flowinfo(),
            address: addr.ip().segments().into(),
            scope_id: addr.scope_id(),
        }),
    }
}

fn from_wasi_ip(address: IpAddress) -> IpAddr {
    match address {
        IpAddress::Ipv4((a, b, c, d)) => IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => {
            IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h))
        }
    }
}

/// An established TCP connection: the two halves of `wasi:sockets` streams
/// over one socket.
///
/// [`Split`] gives the halves up separately; the socket closes when both are
/// dropped.
#[derive(Debug)]
pub struct TcpConnection {
    reader: ReadHalf,
    writer: WriteHalf,
}

/// The receiving half of a [`TcpConnection`].
#[derive(Debug)]
pub struct ReadHalf {
    stream: StreamReader<u8>,
    /// How the stream ended, until it did; taken once it has.
    outcome: Option<FutureReader<Result<(), SocketCode>>>,
    _socket: Rc<TcpSocket>,
}

/// The sending half of a [`TcpConnection`].
#[derive(Debug)]
pub struct WriteHalf {
    /// Dropped to shut the write side down.
    stream: Option<StreamWriter<u8>>,
    /// How the send ended, until it did; taken once it has.
    outcome: Option<FutureReader<Result<(), SocketCode>>>,
    _socket: Rc<TcpSocket>,
}

impl ReadHalf {
    /// The next bytes the peer sent, at most `capacity`; empty at the end of
    /// the stream.
    async fn receive(&mut self, capacity: usize) -> io::Result<Vec<u8>> {
        // Once the stream has ended its outcome is taken, and nothing is read again.
        if self.outcome.is_none() {
            return Ok(Vec::new());
        }
        let (result, bytes) = self.stream.read(Vec::with_capacity(capacity)).await;
        if !stream_closed(result) {
            return Ok(bytes);
        }
        match self.outcome.take() {
            Some(outcome) => outcome.await.map(|()| bytes).map_err(socket_error),
            None => Ok(bytes),
        }
    }
}

impl WriteHalf {
    /// The error a closed send stream stands for: what the host reported, or
    /// a broken pipe if it reported success.
    async fn closed(&mut self) -> io::Error {
        let reported = match self.outcome.take() {
            Some(outcome) => outcome.await.err().map(socket_error),
            None => None,
        };
        reported.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "the connection is closed")
        })
    }

    /// Sends all of `bytes`.
    async fn send(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        let Some(stream) = &mut self.stream else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the write side is shut down",
            ));
        };
        if stream.write_all(bytes).await.is_empty() {
            Ok(())
        } else {
            Err(self.closed().await)
        }
    }
}

impl Read for ReadHalf {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let capacity = buf.as_uninit().len().min(RECEIVE_CHUNK_BYTES);
        if capacity == 0 {
            return BufResult(Ok(0), buf);
        }
        match self.receive(capacity).await {
            Ok(bytes) => {
                let received = bytes.len();
                for (slot, byte) in buf.as_uninit().iter_mut().zip(&bytes) {
                    slot.write(*byte);
                }
                // SAFETY: the first `received` bytes were written just above.
                unsafe { buf.set_len(received) };
                BufResult(Ok(received), buf)
            }
            Err(error) => BufResult(Err(error), buf),
        }
    }
}

impl ReadOwned for ReadHalf {
    type Buf = Vec<u8>;

    /// Lends the vector the stream read into, so no byte is copied.
    async fn read_owned(&mut self) -> io::Result<Received<Vec<u8>>> {
        let bytes = self.receive(RECEIVE_CHUNK_BYTES).await?;
        Ok(if bytes.is_empty() {
            Received::End
        } else {
            Received::Lent(bytes)
        })
    }
}

impl ReadAhead for ReadHalf {
    /// A stream is read only when the reader asks, so nothing is ahead.
    async fn stop_read_ahead(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

/// A connection's bytes cross the `wasi:sockets` streams, never a kernel
/// descriptor a pipe could take them from, so it never splices and is
/// forwarded by reading and writing.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn unsplicable() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a wasi:sockets connection has no descriptor to splice",
    )
}

impl SpliceRead for ReadHalf {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn can_splice(&self) -> bool {
        false
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    async fn splice_to_pipe(
        &mut self,
        pipe: Pipe,
        _max: usize,
    ) -> Result<Spliced, (io::Error, Pipe)> {
        Err((unsplicable(), pipe))
    }
}

impl Write for WriteHalf {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes = buf.as_init().to_vec();
        let written = bytes.len();
        let result = if bytes.is_empty() {
            Ok(0)
        } else {
            self.send(bytes).await.map(|()| written)
        };
        BufResult(result, buf)
    }

    async fn write_vectored<B: IoVectoredBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes: Vec<u8> = buf.iter_slice().flatten().copied().collect();
        let written = bytes.len();
        let result = if bytes.is_empty() {
            Ok(0)
        } else {
            self.send(bytes).await.map(|()| written)
        };
        BufResult(result, buf)
    }

    /// Bytes are handed to the host as they are written.
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Drops the send stream, which `wasi:sockets` defines as
    /// `shutdown(SHUT_WR)`, and waits for the host to say how it went.
    async fn shutdown(&mut self) -> io::Result<()> {
        self.stream = None;
        match self.outcome.take() {
            Some(outcome) => outcome.await.map_err(socket_error),
            None => Ok(()),
        }
    }
}

impl SendFile for WriteHalf {
    /// One positioned read into a buffer, then a write of all of it: the
    /// host has no `sendfile`.
    async fn send_file<F: Borrow<File> + 'static>(
        &mut self,
        file: F,
        offset: u64,
        len: usize,
    ) -> BufResult<usize, F> {
        let mut chunk = vec![0; len.min(RECEIVE_CHUNK_BYTES)];
        let read = {
            // `std::os::wasi::fs::FileExt::read_at` is unstable, so this is a
            // seek and a read.
            let mut file: &File = file.borrow();
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.read(&mut chunk))
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => return BufResult(Err(error), file),
        };
        chunk.truncate(read);
        let result = self.send(chunk).await.map(|()| read);
        BufResult(result, file)
    }
}

impl SpliceWrite for WriteHalf {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn can_splice(&self) -> bool {
        false
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    async fn splice_from_pipe(&mut self, _filled: FilledPipe) -> io::Result<Pipe> {
        Err(unsplicable())
    }
}

impl Split for TcpConnection {
    type ReadHalf = ReadHalf;
    type WriteHalf = WriteHalf;

    fn split(self) -> (ReadHalf, WriteHalf) {
        (self.reader, self.writer)
    }
}

impl Read for TcpConnection {
    fn read<B: IoBufMut>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        self.reader.read(buf)
    }
}

impl ReadOwned for TcpConnection {
    type Buf = Vec<u8>;

    fn read_owned(&mut self) -> impl Future<Output = io::Result<Received<Vec<u8>>>> {
        self.reader.read_owned()
    }
}

impl ReadAhead for TcpConnection {
    fn stop_read_ahead(&mut self) -> impl Future<Output = Vec<u8>> {
        self.reader.stop_read_ahead()
    }
}

impl Write for TcpConnection {
    fn write<B: IoBuf>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        self.writer.write(buf)
    }

    fn write_vectored<B: IoVectoredBuf>(
        &mut self,
        buf: B,
    ) -> impl Future<Output = BufResult<usize, B>> {
        self.writer.write_vectored(buf)
    }

    fn flush(&mut self) -> impl Future<Output = io::Result<()>> {
        self.writer.flush()
    }

    fn shutdown(&mut self) -> impl Future<Output = io::Result<()>> {
        self.writer.shutdown()
    }
}

impl SendFile for TcpConnection {
    fn send_file<F: Borrow<File> + 'static>(
        &mut self,
        file: F,
        offset: u64,
        len: usize,
    ) -> impl Future<Output = BufResult<usize, F>> {
        self.writer.send_file(file, offset, len)
    }
}

impl SpliceRead for TcpConnection {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn can_splice(&self) -> bool {
        SpliceRead::can_splice(&self.reader)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn splice_to_pipe(
        &mut self,
        pipe: Pipe,
        max: usize,
    ) -> impl Future<Output = Result<Spliced, (io::Error, Pipe)>> {
        self.reader.splice_to_pipe(pipe, max)
    }
}

impl SpliceWrite for TcpConnection {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn can_splice(&self) -> bool {
        SpliceWrite::can_splice(&self.writer)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn splice_from_pipe(&mut self, filled: FilledPipe) -> impl Future<Output = io::Result<Pipe>> {
        self.writer.splice_from_pipe(filled)
    }
}

impl TcpOptions for TcpConnection {
    /// The Helios network stack has no Nagle algorithm: segments are sent as
    /// they are written, which is what `TCP_NODELAY` asks for.
    fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        if nodelay {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the Helios network stack always sends without delay",
            ))
        }
    }

    fn nodelay(&self) -> io::Result<bool> {
        Ok(true)
    }
}

/// Runs a client's local tasks on the component's executor.
///
/// A task polls on the component-model executor that drives the export
/// calling it, and keeps being polled after that call returns, as long as
/// something is waiting on it.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeliosExecutor;

impl LocalExecutor for HeliosExecutor {
    type Task<T: 'static> = AsyncTask<T>;

    fn spawn_local<Fut>(&self, future: Fut) -> AsyncTask<Fut::Output>
    where
        Fut: Future + 'static,
    {
        // Each wake-up schedules the task's next poll as a task of the
        // component's own executor.
        let (runnable, task) = spawn_local(future, |runnable| {
            crate::task::spawn(async move {
                runnable.run();
            });
        });
        runnable.schedule();
        task
    }
}
