use std::io::Write as _;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context as _, Result};
use helios_inspector_protocol::system::stats;

use crate::runtime;
use crate::serial::{RpcClient, SerialIo, SerialReader};

const BOOT_SYNC_TIMEOUT: Duration = Duration::from_secs(900);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const DEBUGGER_RUN_STAGE: &str = "run:begin";

pub(crate) async fn connect_after_boot(io: SerialIo) -> Result<RpcClient> {
    let (mut read, write) = io.into_split();
    runtime::timeout(BOOT_SYNC_TIMEOUT, wait_for_debugger_stage(&mut read))
        .await
        .context("timed out waiting for the embedded debugger cold-start markers")??;

    let mut client = helios_inspector_protocol::transport::Client::new(read, write);
    wait_until_ready(&mut client).await?;
    Ok(client)
}

pub(crate) async fn wait_until_ready(client: &mut RpcClient) -> Result<()> {
    runtime::timeout(READY_PROBE_TIMEOUT, stats::snapshot(client))
        .await
        .context("timed out waiting for remote stats readiness probe")?
        .context(
            "failed to fetch initial remote stats snapshot while waiting for debugger readiness",
        )?;
    Ok(())
}

async fn wait_for_debugger_stage(read: &mut SerialReader) -> Result<()> {
    let mut line = Vec::new();

    loop {
        let byte = read_byte(read)
            .await
            .context("failed to read kernel boot markers from the debug serial link")?;
        let Some(byte) = byte else {
            anyhow::bail!(
                "debug serial link closed before the embedded debugger entered wasi:cli/run"
            );
        };

        match byte {
            b'\n' => {
                if let Some(stage) = parse_stage_marker(&line)? {
                    report_stage(stage)?;
                    if stage == DEBUGGER_RUN_STAGE {
                        return Ok(());
                    }
                }
                line.clear();
            }
            b'\r' => {}
            other => line.push(other),
        }
    }
}

async fn read_byte(read: &mut SerialReader) -> std::io::Result<Option<u8>> {
    let mut byte = [0_u8; 1];
    let count = std::future::poll_fn(|cx| Pin::new(&mut **read).poll_read(cx, &mut byte)).await?;
    Ok((count != 0).then_some(byte[0]))
}

fn parse_stage_marker(line: &[u8]) -> Result<Option<&str>> {
    let text =
        std::str::from_utf8(line).context("debug serial preamble contained non-utf8 bytes")?;
    let Some(start) = text.find("[KDBG ") else {
        return Ok(None);
    };
    let text = &text[start..];
    let Some(end) = text.find(']') else {
        anyhow::bail!("malformed debugger stage marker {text:?}");
    };
    let marker = &text[..=end];
    let rest = &text[end + 1..];
    if rest.contains("[KDBG ") {
        anyhow::bail!("multiple debugger stage markers appeared on one serial line: {text:?}");
    }
    let Some(stage) = marker.strip_prefix("[KDBG ") else {
        unreachable!("stage marker search returned a non-marker prefix");
    };
    let Some(stage) = stage.strip_suffix(']') else {
        anyhow::bail!("malformed debugger stage marker {marker:?}");
    };
    Ok(Some(stage))
}

fn report_stage(stage: &str) -> Result<()> {
    let detail = match stage {
        "boot" => "embedded debugger hart booted",
        "engine:new" => "creating Wasmtime engine",
        "engine:ok" => "compiling debugger component",
        "component:ok" => "preparing linker",
        "linker:new" => "building linker",
        "linker:ok" => "creating store",
        "store:new" => "allocating store",
        "store:ok" => "instantiating component",
        "pre:begin" => "running instantiate_pre",
        "pre:ok" => "instantiating component",
        "instantiate:begin" => "instantiating component",
        "instantiate:ok" => "entering wasi:cli/run",
        "run:begin" => "embedded debugger is accepting RPC",
        "run:ok" => "embedded debugger exited",
        "done" => "embedded debugger shut down",
        _ => stage,
    };

    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "helios-inspector: {detail}")?;
    Ok(())
}
