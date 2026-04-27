use std::sync::Arc;

use crate::Error;
use crate::transport_io;
use helios_api::serial;

pub async fn serve_debugger() -> Result<(), Error> {
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
