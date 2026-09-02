//! The host end of the guest's vsock link.
//!
//! `AF_VSOCK` exists on Linux and nowhere else this inspector runs, and
//! even on Linux QEMU can only give a guest a vsock device through
//! `/dev/vhost-vsock`. Both facts are checked before a VM is built
//! rather than discovered as a confusing QEMU failure, and neither is
//! ever papered over: a session asked for the vsock transport on a host
//! that cannot provide it fails with [`VsockUnsupported`] instead of
//! quietly running on the serial line.

use anyhow::Result;

use crate::serial::{RpcReader, RpcWriter};

/// How long the host retries the connection after the guest debugger
/// announced it entered `wasi:cli/run`.
///
/// The marker says the component started, not that it has reached its
/// `listen` call, so the first connection attempts legitimately find
/// nothing bound yet.
#[cfg(target_os = "linux")]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(target_os = "linux")]
const CONNECT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Why this host cannot carry the inspector RPC over vsock.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VsockUnsupported {
    #[error(
        "vsock needs an AF_VSOCK host socket, which only Linux provides; \
         run this session with --rpc-transport serial"
    )]
    HostOperatingSystem,
    #[error(
        "vsock needs {VHOST_VSOCK_DEVICE_NAME}, which this host does not expose; \
         load the vhost_vsock kernel module (modprobe vhost_vsock) or run this \
         session with --rpc-transport serial"
    )]
    // Only the Linux preflight can construct this, but the message is
    // part of the typed contract on every host: the tests below pin it
    // and the documentation quotes it wherever the inspector is built.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    VhostDeviceMissing,
}

/// Named separately so the message above renders the same path the
/// preflight checks, on every host.
const VHOST_VSOCK_DEVICE_NAME: &str = "/dev/vhost-vsock";

/// Checks that this host can give a guest a vsock device at all.
///
/// Called before QEMU is built so an unusable request fails with an
/// explanation rather than as a QEMU device error.
#[cfg(target_os = "linux")]
pub(crate) fn preflight() -> Result<(), VsockUnsupported> {
    if !std::path::Path::new(VHOST_VSOCK_DEVICE_NAME).exists() {
        return Err(VsockUnsupported::VhostDeviceMissing);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn preflight() -> Result<(), VsockUnsupported> {
    Err(VsockUnsupported::HostOperatingSystem)
}

/// Opens the RPC transport to `port` on the guest at `cid`.
#[cfg(target_os = "linux")]
pub(crate) async fn connect(cid: u32, port: u32) -> Result<(RpcReader, RpcWriter)> {
    use anyhow::Context as _;
    use std::io;
    use std::time::Instant;

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let stream = loop {
        match vsock::VsockStream::connect_with_cid_port(cid, port) {
            Ok(stream) => break stream,
            // The guest binds its listener a moment after the component
            // starts, so a refusal before the deadline is "not yet".
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::AddrNotAvailable
                ) && Instant::now() < deadline =>
            {
                async_io::Timer::after(CONNECT_POLL).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to connect to guest vsock cid {cid} port {port}")
                });
            }
        }
    };
    stream
        .set_nonblocking(true)
        .context("failed to configure the guest vsock stream nonblocking")?;
    let read = stream
        .try_clone()
        .context("failed to clone the guest vsock stream reader")?;
    Ok((
        Box::new(
            async_io::Async::new(AsyncVsockStream::new(read))
                .context("failed to register the guest vsock stream reader")?,
        ) as RpcReader,
        Box::new(
            async_io::Async::new(AsyncVsockStream::new(stream))
                .context("failed to register the guest vsock stream writer")?,
        ) as RpcWriter,
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn connect(cid: u32, port: u32) -> Result<(RpcReader, RpcWriter)> {
    let _ = (cid, port);
    Err(VsockUnsupported::HostOperatingSystem.into())
}

#[cfg(target_os = "linux")]
struct AsyncVsockStream {
    stream: vsock::VsockStream,
}

#[cfg(target_os = "linux")]
impl AsyncVsockStream {
    fn new(stream: vsock::VsockStream) -> Self {
        Self { stream }
    }
}

#[cfg(target_os = "linux")]
unsafe impl async_io::IoSafe for AsyncVsockStream {}

#[cfg(target_os = "linux")]
impl std::os::fd::AsFd for AsyncVsockStream {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsRawFd as _;
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.stream.as_raw_fd()) }
    }
}

#[cfg(target_os = "linux")]
impl std::io::Read for AsyncVsockStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.stream, buf)
    }
}

#[cfg(target_os = "linux")]
impl std::io::Write for AsyncVsockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut self.stream, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.stream)
    }
}

/// A context id for one guest.
///
/// The hypervisor rejects a duplicate, so the value is derived from the
/// inspector process rather than fixed: two sessions on one host get
/// different ids, and a collision fails QEMU startup loudly instead of
/// silently attaching to the wrong machine. Ids below 3 are reserved.
pub(crate) fn default_guest_cid() -> u32 {
    const FIRST_GUEST_CID: u32 = 3;
    /// Leaves the reserved ids alone and stays inside the range a
    /// 32-bit context id can hold.
    const CID_SPAN: u32 = u32::MAX - FIRST_GUEST_CID - 1;
    FIRST_GUEST_CID + (std::process::id() % CID_SPAN)
}

#[cfg(test)]
mod tests {
    use super::{VsockUnsupported, default_guest_cid, preflight};

    #[test]
    fn every_refusal_names_the_serial_transport_as_the_way_forward() {
        // Both messages are the only thing an operator sees when a
        // session cannot use vsock, so each has to say what to do next
        // rather than only what went wrong.
        for refusal in [
            VsockUnsupported::HostOperatingSystem,
            VsockUnsupported::VhostDeviceMissing,
        ] {
            let message = refusal.to_string();
            assert!(
                message.contains("--rpc-transport serial"),
                "refusal does not name the transport that works here: {message}"
            );
        }
        assert!(
            VsockUnsupported::VhostDeviceMissing
                .to_string()
                .contains("/dev/vhost-vsock"),
            "a missing vhost device has to name the device node"
        );
    }

    #[test]
    fn a_guest_context_id_never_lands_on_a_reserved_one() {
        // 0, 1 and 2 belong to the hypervisor, the retired loopback
        // address, and the host itself.
        assert!(default_guest_cid() >= 3);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn a_host_without_af_vsock_refuses_rather_than_falling_back() {
        assert!(matches!(
            preflight(),
            Err(VsockUnsupported::HostOperatingSystem)
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_linux_host_is_judged_by_its_vhost_device() {
        let expected = std::path::Path::new("/dev/vhost-vsock").exists();
        assert_eq!(preflight().is_ok(), expected);
    }
}
