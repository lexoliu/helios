use std::sync::Arc;

use crate::Error;
use crate::transport_io::{self, EndpointReader, EndpointWriter};
use helios_api::serial;
use helios_api::vsock::{self, VsockListener, VsockStream};
use helios_inspector_protocol::VSOCK_RPC_PORT;
use helios_inspector_protocol::system::{
    self,
    accept::{Connection, Listener, SessionFault},
};

/// Connections the listener queues while the debugger is serving one.
///
/// The inspector opens a single RPC connection at a time, but a session
/// that dropped its transport and reconnected must not be refused while
/// the previous connection is still being torn down.
const VSOCK_BACKLOG: u32 = 2;

pub async fn serve_debugger() -> Result<(), Error> {
    // vsock is the transport wherever the machine has the device: the
    // serial line is shared with console output and the boot log, so RPC
    // framing on it competes with everything else the kernel prints.
    // A machine without the device keeps the serial transport, which is
    // a different transport for the same protocol, not a degraded one.
    match vsock::guest_cid().await {
        Some(guest_cid) => serve_over_vsock(guest_cid).await,
        None => serve_over_serial().await,
    }
}

async fn serve_over_vsock(guest_cid: u64) -> Result<(), Error> {
    let mut listener = VsockListener::bind(VSOCK_RPC_PORT, VSOCK_BACKLOG)
        .await
        .map_err(Error::VsockTransport)?;
    // Neither the wait for the inspector to attach nor an idle control
    // connection has a deadline: a debugger that gave up on a quiet
    // session would be unusable exactly when it is needed.
    listener.set_timeout(vsock::NO_DEADLINE);
    println!("debugger listening on vsock cid {guest_cid} port {VSOCK_RPC_PORT}");

    // A session that fails is the peer's fault and ends with a report;
    // only the listener failing to accept ends the debugger.
    match system::serve_listener(VsockSessions(listener), report_session_fault).await {
        Ok(never) => match never {},
        Err(error) => Err(Error::VsockTransport(error)),
    }
}

fn report_session_fault(fault: SessionFault) {
    match fault {
        SessionFault::Failed(error) => println!("debugger vsock session ended: {error}"),
        SessionFault::CloseFailed(error) => {
            println!("debugger vsock session could not be closed: {error}");
        }
    }
}

/// The vsock listener as the accept loop's source of connections.
struct VsockSessions(VsockListener);

impl Listener for VsockSessions {
    type Connection = VsockConnection;
    type Error = std::io::Error;

    async fn accept(&mut self) -> std::io::Result<VsockConnection> {
        Ok(VsockConnection(Arc::new(self.0.accept().await?)))
    }
}

struct VsockConnection(Arc<VsockStream>);

impl Connection for VsockConnection {
    type Read = EndpointReader<VsockStream>;
    type Write = EndpointWriter<VsockStream>;

    fn halves(&mut self) -> (Self::Read, Self::Write) {
        transport_io::split(Arc::clone(&self.0), Arc::clone(&self.0))
    }

    async fn close(self) -> std::io::Result<()> {
        self.0.close().await
    }
}

async fn serve_over_serial() -> Result<(), Error> {
    let read_port = Arc::new(
        serial::debug_port()
            .await
            .ok_or(Error::MissingSerialCapability)?,
    );
    let write_port = Arc::new(
        serial::debug_port()
            .await
            .ok_or(Error::MissingSerialCapability)?,
    );
    let (read, write) = transport_io::split(read_port, write_port);
    system::serve(read, write).await.map_err(Error::from)
}
