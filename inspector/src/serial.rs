use anyhow::{Context as _, Result};
use async_io::Async;
use async_net::unix::UnixStream as AsyncUnixStream;
use futures_util::io::AsyncReadExt as _;
use futures_io::{AsyncRead, AsyncWrite};
use std::io;
use std::time::{Duration, Instant};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::FileTypeExt;

pub(crate) trait SerialRead: AsyncRead + Unpin + Send {}
pub(crate) trait SerialWrite: AsyncWrite + Unpin + Send {}

impl<T> SerialRead for T where T: AsyncRead + Unpin + Send {}
impl<T> SerialWrite for T where T: AsyncWrite + Unpin + Send {}

pub(crate) type SerialReader = Box<dyn SerialRead>;
pub(crate) type SerialWriter = Box<dyn SerialWrite>;
pub(crate) type RpcClient =
    helios_inspector_protocol::transport::Client<SerialReader, SerialWriter>;

const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_CONNECT_POLL: Duration = Duration::from_millis(50);

pub(crate) struct SerialIo {
    read: SerialReader,
    write: SerialWriter,
}

pub(crate) async fn open(device: &str, baud: u32) -> Result<SerialIo> {
    let (read, write) = if is_unix_socket(device)? {
        open_socket_transport(device).await?
    } else {
        open_tty_transport(device, baud)?
    };
    Ok(SerialIo { read, write })
}

fn is_unix_socket(device: &str) -> Result<bool> {
    match std::fs::metadata(device) {
        Ok(metadata) => Ok(metadata.file_type().is_socket()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(device.ends_with(".sock")),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect serial device {device}"))
        }
    }
}

impl SerialIo {
    pub(crate) fn into_client(self) -> RpcClient {
        let (read, write) = self.into_split();
        helios_inspector_protocol::transport::Client::new(read, write)
    }

    pub(crate) fn into_split(self) -> (SerialReader, SerialWriter) {
        (self.read, self.write)
    }
}

struct AsyncSerialPort {
    port: serialport::TTYPort,
}

impl AsyncSerialPort {
    fn new(port: serialport::TTYPort) -> Self {
        Self { port }
    }
}

unsafe impl async_io::IoSafe for AsyncSerialPort {}

impl AsFd for AsyncSerialPort {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.port.as_raw_fd()) }
    }
}

impl io::Read for AsyncSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.port.read(buf)
    }
}

impl io::Write for AsyncSerialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.port.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.port.flush()
    }
}

fn open_tty_transport(device: &str, baud: u32) -> Result<(SerialReader, SerialWriter)> {
    let port = serialport::new(device, baud)
        .open_native()
        .with_context(|| format!("failed to open serial device {device} with serialport"))?;
    let read_port = port
        .try_clone_native()
        .with_context(|| format!("failed to clone serial device {device}"))?;
    Ok((
        Box::new(
            Async::new(AsyncSerialPort::new(read_port))
                .with_context(|| format!("failed to register serial device {device}"))?,
        ) as SerialReader,
        Box::new(
            Async::new(AsyncSerialPort::new(port))
                .with_context(|| format!("failed to register serial device {device}"))?,
        ) as SerialWriter,
    ))
}

async fn open_socket_transport(device: &str) -> Result<(SerialReader, SerialWriter)> {
    let deadline = Instant::now() + SOCKET_CONNECT_TIMEOUT;
    let socket = loop {
        match AsyncUnixStream::connect(device).await {
            Ok(socket) => break socket,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::AddrNotAvailable
                ) && Instant::now() < deadline =>
            {
                async_io::Timer::after(SOCKET_CONNECT_POLL).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to connect to serial socket {device}"));
            }
        }
    };
    let (read, write) = socket.split();
    Ok((Box::new(read) as SerialReader, Box::new(write) as SerialWriter))
}
