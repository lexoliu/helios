use anyhow::{Context as _, Result};
use async_io::Async;
use async_net::unix::UnixStream;
use futures_io::{AsyncRead, AsyncWrite};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::FileTypeExt;

pub(crate) trait SerialRead: AsyncRead + Unpin + Send {}
pub(crate) trait SerialWrite: AsyncWrite + Unpin + Send {}

impl<T> SerialRead for T where T: AsyncRead + Unpin + Send {}
impl<T> SerialWrite for T where T: AsyncWrite + Unpin + Send {}

pub(crate) type SerialReader = Box<dyn SerialRead>;
pub(crate) type SerialWriter = Box<dyn SerialWrite>;
pub(crate) type RpcClient = helios_shell_protocol::transport::Client<SerialReader, SerialWriter>;

pub(crate) struct SerialIo {
    read: SerialReader,
    write: SerialWriter,
}

pub(crate) async fn open(device: &str, baud: u32) -> Result<SerialIo> {
    let (read, write) = if is_unix_socket(device)? {
        let stream = UnixStream::connect(device)
            .await
            .with_context(|| format!("failed to connect to serial socket {device}"))?;
        (
            Box::new(stream.clone()) as SerialReader,
            Box::new(stream) as SerialWriter,
        )
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
        helios_shell_protocol::transport::Client::new(read, write)
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

struct AsyncRawTty {
    file: File,
}

impl AsyncRawTty {
    fn open(device: &str, baud: u32) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(device)
            .with_context(|| format!("failed to open tty device {device}"))?;
        configure_raw_tty(&file, baud)
            .with_context(|| format!("failed to configure tty device {device}"))?;
        Ok(Self { file })
    }

    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            file: self
                .file
                .try_clone()
                .context("failed to duplicate raw tty file descriptor")?,
        })
    }
}

unsafe impl async_io::IoSafe for AsyncRawTty {}

impl AsFd for AsyncRawTty {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl io::Read for AsyncRawTty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl io::Write for AsyncRawTty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn open_tty_transport(device: &str, baud: u32) -> Result<(SerialReader, SerialWriter)> {
    match serialport::new(device, baud).open_native() {
        Ok(port) => {
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
        Err(error) => {
            let raw_tty = AsyncRawTty::open(device, baud).with_context(|| {
                format!(
                    "failed to open serial device {device} via serialport ({error}) and raw tty"
                )
            })?;
            let read_tty = raw_tty
                .try_clone()
                .with_context(|| format!("failed to clone tty device {device}"))?;
            Ok((
                Box::new(Async::new(read_tty).with_context(|| {
                    format!("failed to register tty device {device} for reading")
                })?) as SerialReader,
                Box::new(Async::new(raw_tty).with_context(|| {
                    format!("failed to register tty device {device} for writing")
                })?) as SerialWriter,
            ))
        }
    }
}

fn configure_raw_tty(file: &File, baud: u32) -> io::Result<()> {
    use rustix::termios::{tcgetattr, tcsetattr, OptionalActions};

    let mut termios = tcgetattr(file)?;
    termios.make_raw();
    termios.set_input_speed(baud)?;
    termios.set_output_speed(baud)?;
    Ok(tcsetattr(file, OptionalActions::Now, &termios)?)
}
