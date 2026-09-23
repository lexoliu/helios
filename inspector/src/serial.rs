use anyhow::{Context as _, Result};
use async_io::Async;
use futures_io::{AsyncRead, AsyncWrite};
use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout};
use std::time::{Duration, Instant};

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

pub(crate) struct PtyTransport {
    pub(crate) io: SerialIo,
    pub(crate) slave_path: PathBuf,
    pub(crate) slave: File,
}

pub(crate) async fn open(device: &str, baud: u32) -> Result<SerialIo> {
    let (read, write) = if is_unix_socket(device)? {
        open_socket_transport(device).await?
    } else {
        open_tty_transport(device, baud)?
    };
    Ok(SerialIo { read, write })
}

pub(crate) fn open_child_stdio(stdout: ChildStdout, stdin: ChildStdin) -> Result<SerialIo> {
    Ok(SerialIo {
        read: Box::new(
            Async::new(AsyncChildStdout::new(stdout))
                .context("failed to register QEMU stdout serial pipe")?,
        ),
        write: Box::new(
            Async::new(AsyncChildStdin::new(stdin))
                .context("failed to register QEMU stdin serial pipe")?,
        ),
    })
}

pub(crate) fn open_pty_transport() -> Result<PtyTransport> {
    let mut master_fd = 0;
    let mut slave_fd = 0;
    // `c_char` is i8 on x86-64 Linux but u8 on aarch64 Linux; spell the
    // libc type so the buffer matches on both.
    let mut name = [0 as libc::c_char; 128];
    let status = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            name.as_mut_ptr(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error()).context("failed to create serial PTY");
    }
    configure_raw_pty(slave_fd)?;
    let path = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .context("serial PTY path was not valid UTF-8")?;
    configure_nonblocking(master_fd)?;
    let master = unsafe { File::from_raw_fd(master_fd) };
    let read = master
        .try_clone()
        .context("failed to clone serial PTY master")?;
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    Ok(PtyTransport {
        io: SerialIo {
            read: Box::new(
                Async::new(PtyFile::new(read)).context("failed to register serial PTY reader")?,
            ),
            write: Box::new(
                Async::new(PtyFile::new(master)).context("failed to register serial PTY writer")?,
            ),
        },
        slave_path: PathBuf::from(path),
        slave,
    })
}

fn configure_raw_pty(fd: libc::c_int) -> Result<()> {
    let mut termios = core::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("failed to inspect serial PTY termios");
    }
    let mut termios = unsafe { termios.assume_init() };
    unsafe {
        libc::cfmakeraw(&mut termios);
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(io::Error::last_os_error()).context("failed to configure serial PTY raw mode");
    }
    Ok(())
}

fn configure_nonblocking(fd: libc::c_int) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("failed to inspect serial PTY flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error())
            .context("failed to configure serial PTY nonblocking");
    }
    Ok(())
}

struct PtyFile {
    file: File,
}

impl PtyFile {
    fn new(file: File) -> Self {
        Self { file }
    }
}

unsafe impl async_io::IoSafe for PtyFile {}

impl AsFd for PtyFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl io::Read for PtyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.file, buf)
    }
}

impl io::Write for PtyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.file, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.file)
    }
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
        io::Read::read(&mut self.port, buf)
    }
}

impl io::Write for AsyncSerialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.port, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.port)
    }
}

struct AsyncChildStdout {
    stdout: ChildStdout,
}

impl AsyncChildStdout {
    fn new(stdout: ChildStdout) -> Self {
        Self { stdout }
    }
}

unsafe impl async_io::IoSafe for AsyncChildStdout {}

impl AsFd for AsyncChildStdout {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }
}

impl io::Read for AsyncChildStdout {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.stdout, buf)
    }
}

struct AsyncChildStdin {
    stdin: ChildStdin,
}

impl AsyncChildStdin {
    fn new(stdin: ChildStdin) -> Self {
        Self { stdin }
    }
}

unsafe impl async_io::IoSafe for AsyncChildStdin {}

impl AsFd for AsyncChildStdin {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stdin.as_fd()
    }
}

impl io::Write for AsyncChildStdin {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.stdin, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.stdin)
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
        match UnixStream::connect(device) {
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
    socket
        .set_nonblocking(true)
        .with_context(|| format!("failed to configure serial socket {device} nonblocking"))?;
    let read = socket
        .try_clone()
        .with_context(|| format!("failed to clone serial socket {device} reader"))?;
    Ok((
        Box::new(
            Async::new(AsyncUnixSocket::new(read))
                .with_context(|| format!("failed to register serial socket {device} reader"))?,
        ) as SerialReader,
        Box::new(
            Async::new(AsyncUnixSocket::new(socket))
                .with_context(|| format!("failed to register serial socket {device} writer"))?,
        ) as SerialWriter,
    ))
}

struct AsyncUnixSocket {
    socket: UnixStream,
}

impl AsyncUnixSocket {
    fn new(socket: UnixStream) -> Self {
        Self { socket }
    }
}

unsafe impl async_io::IoSafe for AsyncUnixSocket {}

impl AsFd for AsyncUnixSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl io::Read for AsyncUnixSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.socket, buf)
    }
}

impl io::Write for AsyncUnixSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.socket, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.socket)
    }
}
