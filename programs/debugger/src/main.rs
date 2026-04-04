mod rpc_server;
mod transport_io;

#[helios_api::main]
async fn main() -> anyhow::Result<()> {
    rpc_server::serve_debugger().await
}
