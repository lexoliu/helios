use std::sync::Arc;

use crate::transport_io;
use helios_api::serial;

pub async fn serve_debugger() -> anyhow::Result<()> {
    let read_port = Arc::new(
        serial::debug_port()
            .await
            .ok_or_else(|| anyhow::anyhow!("debug serial capability is missing"))?,
    );
    let write_port = Arc::new(
        serial::debug_port()
            .await
            .ok_or_else(|| anyhow::anyhow!("debug serial capability is missing"))?,
    );
    let (read, write) = transport_io::split(read_port, write_port);
    helios_inspector_protocol::system::serve(read, write).await
}
