use std::io::Write as _;
use std::time::Duration;

use crate::runtime;
use crate::serial::{RpcClient, SerialIo};
use anyhow::{Context as _, Result};

const INITIAL_REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct RpcPane {
    pub instance: String,
    pub func: String,
    pub request_hex: String,
    pub response_hex: String,
}

impl RpcPane {
    pub fn new() -> Self {
        Self {
            instance: String::new(),
            func: String::new(),
            request_hex: String::new(),
            response_hex: "no rpc response yet".to_owned(),
        }
    }

    pub async fn call(&mut self, client: &mut RpcClient) -> Result<()> {
        let request = decode_hex(&self.request_hex)?;
        let response = runtime::timeout(
            INITIAL_REMOTE_TIMEOUT,
            client.invoke_raw(&self.instance, &self.func, request),
        )
        .await
        .context("timed out waiting for remote rpc response")?
        .with_context(|| {
            format!(
                "failed to invoke remote method {}.{}",
                self.instance, self.func
            )
        })?;
        self.response_hex = hex::encode(response);
        Ok(())
    }
}

pub async fn run_call(io: SerialIo, instance: &str, func: &str, request_hex: &str) -> Result<()> {
    let mut client = io.into_client();
    let mut pane = RpcPane::new();
    pane.instance = instance.to_owned();
    pane.func = func.to_owned();
    pane.request_hex = request_hex.to_owned();
    pane.call(&mut client).await?;
    std::io::stdout().write_all(pane.response_hex.as_bytes())?;
    std::io::stdout().write_all(b"\n")?;
    Ok(())
}

fn decode_hex(hex_text: &str) -> Result<Vec<u8>> {
    if hex_text.is_empty() {
        return Ok(Vec::new());
    }
    hex::decode(hex_text).with_context(|| format!("invalid hex payload {hex_text}"))
}
