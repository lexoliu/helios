//! Host/guest stream sockets over the machine's vsock transport.
//!
//! vsock reaches the machine's host without a network: no interface, no
//! address configuration, no name resolution. A program that has to talk
//! to whatever is running it — the inspector's RPC transport is the one
//! this kernel ships — uses this rather than TCP, so it works on a
//! machine whose network is down or deliberately absent.

use std::io;
use std::time::Duration;
use std::vec::Vec;

use crate::bindings::helios::system::vsock as raw;

pub use crate::bindings::helios::system::vsock::{VsockAddress, VsockErrorKind};

/// Context id of the host end of the link (virtio 1.2 §5.10.4).
///
/// There is exactly one host per machine and its id is fixed by the
/// specification, so a program addresses it directly rather than
/// discovering it.
pub const HOST_CID: u64 = 2;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// A deadline far enough out that an operation waits indefinitely.
///
/// vsock has no idle timeout of its own — a long-lived control
/// connection is idle most of the time — so a server that must not give
/// up on a quiet peer sets this rather than picking a number that will
/// eventually be wrong.
pub const NO_DEADLINE: Duration = Duration::from_nanos(u64::MAX);
/// Default read size used by convenience readers, matched to the payload
/// a handful of vsock packets carry.
pub const DEFAULT_READ_CHUNK_BYTES: usize = 64 * 1024;

/// The context id the hypervisor assigned this machine, or `None` when
/// the machine has no vsock device or the program holds no host-link
/// capability.
pub async fn guest_cid() -> Option<u64> {
    raw::guest_cid().await
}

/// An open vsock connection.
pub struct VsockStream {
    raw: raw::VsockStream,
    timeout: Duration,
}

/// A bound vsock port.
pub struct VsockListener {
    raw: raw::VsockListener,
    timeout: Duration,
}

impl VsockStream {
    /// Connects to `port` on the host using the default timeout.
    pub async fn connect_host(port: u32) -> io::Result<Self> {
        Self::connect(
            VsockAddress {
                cid: HOST_CID,
                port,
            },
            DEFAULT_TIMEOUT,
        )
        .await
    }

    /// Connects to `address`, storing `timeout` as the default for later
    /// reads and writes.
    pub async fn connect(address: VsockAddress, timeout: Duration) -> io::Result<Self> {
        let raw = raw::connect(address, duration_to_nanos(timeout))
            .await
            .map_err(map_error)?;
        Ok(Self { raw, timeout })
    }

    /// The address of the peer on the other end.
    pub async fn peer(&self) -> io::Result<VsockAddress> {
        self.raw.peer().await.map_err(map_error)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Reads one chunk from the peer. `Ok(None)` is an orderly end of
    /// file.
    pub async fn read(&self, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        let max_bytes = u32::try_from(max_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "vsock read size does not fit into u32",
            )
        })?;
        self.raw
            .read(max_bytes, duration_to_nanos(self.timeout))
            .await
            .map_err(map_error)
    }

    /// Writes `bytes` completely, coming back for whatever the peer's
    /// credit window would not take the first time.
    pub async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            let chunk = self
                .raw
                .write(bytes[written..].to_vec(), duration_to_nanos(self.timeout))
                .await
                .map_err(map_error)?;
            let chunk = usize::try_from(chunk)
                .map_err(|_| io::Error::other("vsock write length does not fit into usize"))?;
            if chunk == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "vsock stream accepted no bytes before its deadline",
                ));
            }
            written += chunk;
        }
        Ok(())
    }

    /// Announces that this end will send no more bytes.
    pub async fn shutdown_send(&self) -> io::Result<()> {
        self.raw.shutdown_send().await.map_err(map_error)
    }

    /// Releases the connection immediately.
    pub async fn close(&self) -> io::Result<()> {
        self.raw.close().await.map_err(map_error)
    }
}

impl VsockListener {
    /// Binds `port`, or an ephemeral port when `port` is zero.
    pub async fn bind(port: u32, backlog: u32) -> io::Result<Self> {
        let raw = raw::listen(port, backlog).await.map_err(map_error)?;
        Ok(Self {
            raw,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// The port this listener is bound to.
    pub async fn port(&self) -> io::Result<u32> {
        self.raw.port().await.map_err(map_error)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Waits for the next connection.
    pub async fn accept(&self) -> io::Result<VsockStream> {
        let raw = self
            .raw
            .accept(duration_to_nanos(self.timeout))
            .await
            .map_err(map_error)?;
        Ok(VsockStream {
            raw,
            timeout: self.timeout,
        })
    }

    /// Releases the bound port. Accepted connections stay open.
    pub async fn close(&self) -> io::Result<()> {
        self.raw.close().await.map_err(map_error)
    }
}

fn duration_to_nanos(duration: Duration) -> u64 {
    duration
        .as_nanos()
        .try_into()
        .expect("duration does not fit into wasi nanoseconds")
}

fn map_error(error: raw::VsockError) -> io::Error {
    let kind = match error.kind {
        VsockErrorKind::Unavailable => io::ErrorKind::Unsupported,
        VsockErrorKind::AddressInUse => io::ErrorKind::AddrInUse,
        VsockErrorKind::ConnectionRefused => io::ErrorKind::ConnectionRefused,
        VsockErrorKind::ConnectionReset => io::ErrorKind::ConnectionReset,
        VsockErrorKind::Closed => io::ErrorKind::NotConnected,
        VsockErrorKind::Timeout => io::ErrorKind::TimedOut,
        VsockErrorKind::PermissionDenied => io::ErrorKind::PermissionDenied,
        VsockErrorKind::Internal => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{:?}: {}", error.kind, error.detail))
}
