use std::sync::Arc;

use crate::Error;
use crate::transport_io;
use helios_api::serial;
use helios_api::vsock::{self, VsockListener};
use helios_inspector_protocol::VSOCK_RPC_PORT;

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

    loop {
        let stream = Arc::new(listener.accept().await.map_err(Error::VsockTransport)?);
        let (read, write) = transport_io::split(Arc::clone(&stream), Arc::clone(&stream));
        // One session at a time: the protocol multiplexes its own
        // invocations, so a second transport would be a second view of
        // the same machine rather than more concurrency.
        let outcome = helios_inspector_protocol::system::serve(read, write).await;
        if let Err(error) = stream.close().await {
            println!("debugger vsock session could not be closed: {error}");
        }
        outcome.map_err(Error::from)?;
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
    helios_inspector_protocol::system::serve(read, write)
        .await
        .map_err(Error::from)
}
