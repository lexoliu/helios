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

/// Which half of a transport a failure concerns.
///
/// A transport is opened as a pair, and the two halves fail for
/// different reasons — a reader that cannot be cloned and a writer that
/// cannot be registered are different defects — so the half is part of
/// what the message names.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TransportHalf {
    Reader,
    Writer,
}

impl std::fmt::Display for TransportHalf {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
        })
    }
}

/// Why the inspector could not open the transport its RPC runs over.
///
/// Each variant names the device or pipe it was working on and the step
/// that failed, because the caller's next move differs by step: a device
/// that cannot be opened is a wrong path or a permission, a device that
/// opens and cannot be registered is a host that will not poll it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SerialError {
    #[error("failed to inspect serial device {device}: {source}")]
    InspectDevice {
        device: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to open serial device {device} with serialport: {source}")]
    OpenTty {
        device: String,
        #[source]
        source: serialport::Error,
    },
    #[error("failed to clone serial device {device}: {source}")]
    CloneTty {
        device: String,
        #[source]
        source: serialport::Error,
    },
    #[error("failed to register serial device {device}: {source}")]
    RegisterTty {
        device: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to connect to serial socket {device}: {source}")]
    ConnectSocket {
        device: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to configure serial socket {device} nonblocking: {source}")]
    ConfigureSocket {
        device: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to clone serial socket {device} reader: {source}")]
    CloneSocket {
        device: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to register serial socket {device} {half}: {source}")]
    RegisterSocket {
        device: String,
        half: TransportHalf,
        #[source]
        source: io::Error,
    },
    #[error("failed to register QEMU {pipe} serial pipe: {source}")]
    RegisterChildPipe {
        pipe: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to create serial PTY: {source}")]
    CreatePty {
        #[source]
        source: io::Error,
    },
    #[error("failed to {step}: {source}")]
    ConfigurePty {
        /// The PTY attribute that was being read or written, spelled the
        /// way the message reads it.
        step: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("serial PTY path was not valid UTF-8: {source}")]
    PtyPathNotUtf8 {
        #[source]
        source: core::str::Utf8Error,
    },
    #[error("failed to clone serial PTY master: {source}")]
    ClonePty {
        #[source]
        source: io::Error,
    },
    #[error("failed to register serial PTY {half}: {source}")]
    RegisterPty {
        half: TransportHalf,
        #[source]
        source: io::Error,
    },
}

/// The read half of a transport the inspector RPC can run over.
///
/// The transport is deliberately erased: the RPC framing is identical
/// whether it runs over a serial line, a QEMU stdio pipe, or a vsock
/// connection, so nothing above this alias is written twice per
/// transport.
pub(crate) trait RpcRead: AsyncRead + Unpin + Send {}
pub(crate) trait RpcWrite: AsyncWrite + Unpin + Send {}

impl<T> RpcRead for T where T: AsyncRead + Unpin + Send {}
impl<T> RpcWrite for T where T: AsyncWrite + Unpin + Send {}

pub(crate) type RpcReader = Box<dyn RpcRead>;
pub(crate) type RpcWriter = Box<dyn RpcWrite>;
pub(crate) type RpcClient = helios_inspector_protocol::transport::Client<RpcReader, RpcWriter>;

const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_CONNECT_POLL: Duration = Duration::from_millis(50);

pub(crate) struct SerialIo {
    read: RpcReader,
    write: RpcWriter,
}

pub(crate) struct PtyTransport {
    pub(crate) io: SerialIo,
    pub(crate) slave_path: PathBuf,
    pub(crate) slave: File,
}

pub(crate) async fn open(device: &str, baud: u32) -> Result<SerialIo, SerialError> {
    let (read, write) = if is_unix_socket(device)? {
        open_socket_transport(device).await?
    } else {
        open_tty_transport(device, baud)?
    };
    Ok(SerialIo { read, write })
}

pub(crate) fn open_child_stdio(
    stdout: ChildStdout,
    stdin: ChildStdin,
) -> Result<SerialIo, SerialError> {
    Ok(SerialIo {
        read: Box::new(Async::new(AsyncChildStdout::new(stdout)).map_err(|source| {
            SerialError::RegisterChildPipe {
                pipe: "stdout",
                source,
            }
        })?),
        write: Box::new(Async::new(AsyncChildStdin::new(stdin)).map_err(|source| {
            SerialError::RegisterChildPipe {
                pipe: "stdin",
                source,
            }
        })?),
    })
}

pub(crate) fn open_pty_transport() -> Result<PtyTransport, SerialError> {
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
        return Err(SerialError::CreatePty {
            source: io::Error::last_os_error(),
        });
    }
    configure_raw_pty(slave_fd)?;
    let path = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .map_err(|source| SerialError::PtyPathNotUtf8 { source })?;
    configure_nonblocking(master_fd)?;
    let master = unsafe { File::from_raw_fd(master_fd) };
    let read = master
        .try_clone()
        .map_err(|source| SerialError::ClonePty { source })?;
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    Ok(PtyTransport {
        io: SerialIo {
            read: Box::new(Async::new(PtyFile::new(read)).map_err(|source| {
                SerialError::RegisterPty {
                    half: TransportHalf::Reader,
                    source,
                }
            })?),
            write: Box::new(Async::new(PtyFile::new(master)).map_err(|source| {
                SerialError::RegisterPty {
                    half: TransportHalf::Writer,
                    source,
                }
            })?),
        },
        slave_path: PathBuf::from(path),
        slave,
    })
}

fn configure_raw_pty(fd: libc::c_int) -> Result<(), SerialError> {
    let mut termios = core::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return Err(SerialError::ConfigurePty {
            step: "inspect serial PTY termios",
            source: io::Error::last_os_error(),
        });
    }
    let mut termios = unsafe { termios.assume_init() };
    unsafe {
        libc::cfmakeraw(&mut termios);
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(SerialError::ConfigurePty {
            step: "configure serial PTY raw mode",
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn configure_nonblocking(fd: libc::c_int) -> Result<(), SerialError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(SerialError::ConfigurePty {
            step: "inspect serial PTY flags",
            source: io::Error::last_os_error(),
        });
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(SerialError::ConfigurePty {
            step: "configure serial PTY nonblocking",
            source: io::Error::last_os_error(),
        });
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

fn is_unix_socket(device: &str) -> Result<bool, SerialError> {
    match std::fs::metadata(device) {
        Ok(metadata) => Ok(metadata.file_type().is_socket()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(device.ends_with(".sock")),
        Err(source) => Err(SerialError::InspectDevice {
            device: device.to_owned(),
            source,
        }),
    }
}

impl SerialIo {
    pub(crate) fn into_client(self) -> RpcClient {
        let (read, write) = self.into_split();
        helios_inspector_protocol::transport::Client::new(read, write)
    }

    pub(crate) fn into_split(self) -> (RpcReader, RpcWriter) {
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

fn open_tty_transport(device: &str, baud: u32) -> Result<(RpcReader, RpcWriter), SerialError> {
    let register = |source| SerialError::RegisterTty {
        device: device.to_owned(),
        source,
    };
    let port = serialport::new(device, baud)
        .open_native()
        .map_err(|source| SerialError::OpenTty {
            device: device.to_owned(),
            source,
        })?;
    let read_port = port
        .try_clone_native()
        .map_err(|source| SerialError::CloneTty {
            device: device.to_owned(),
            source,
        })?;
    Ok((
        Box::new(Async::new(AsyncSerialPort::new(read_port)).map_err(register)?) as RpcReader,
        Box::new(Async::new(AsyncSerialPort::new(port)).map_err(register)?) as RpcWriter,
    ))
}

async fn open_socket_transport(device: &str) -> Result<(RpcReader, RpcWriter), SerialError> {
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
            Err(source) => {
                return Err(SerialError::ConnectSocket {
                    device: device.to_owned(),
                    source,
                });
            }
        }
    };
    socket
        .set_nonblocking(true)
        .map_err(|source| SerialError::ConfigureSocket {
            device: device.to_owned(),
            source,
        })?;
    let read = socket
        .try_clone()
        .map_err(|source| SerialError::CloneSocket {
            device: device.to_owned(),
            source,
        })?;
    let register = |half| {
        move |source| SerialError::RegisterSocket {
            device: device.to_owned(),
            half,
            source,
        }
    };
    Ok((
        Box::new(Async::new(AsyncUnixSocket::new(read)).map_err(register(TransportHalf::Reader))?)
            as RpcReader,
        Box::new(Async::new(AsyncUnixSocket::new(socket)).map_err(register(TransportHalf::Writer))?)
            as RpcWriter,
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
