use std::io::Write as _;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context as _, Result};
use helios_inspector_protocol::system::stats;

use crate::runtime;
use crate::serial::{RpcClient, RpcReader, SerialIo};

const BOOT_SYNC_TIMEOUT: Duration = Duration::from_secs(900);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const READY_DRAIN_QUIET_PERIOD: Duration = Duration::from_millis(100);
const DEBUGGER_RUN_STAGE: &str = "run:begin";

/// Boot-marker deadline. Firmware loading a release kernel image under
/// pure TCG can legitimately take longer than the default 15 minutes,
/// so slow hosts may widen it via HELIOS_BOOT_SYNC_TIMEOUT_SECS.
fn boot_sync_timeout() -> Duration {
    std::env::var("HELIOS_BOOT_SYNC_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(BOOT_SYNC_TIMEOUT)
}

pub(crate) async fn connect_after_boot(io: SerialIo) -> Result<RpcClient> {
    let (mut read, write) = io.into_split();
    wait_for_boot(&mut read).await?;
    let mut client = helios_inspector_protocol::transport::Client::new(read, write);
    wait_until_ready(&mut client).await?;
    Ok(client)
}

/// Waits on the guest's serial line until the embedded debugger reports
/// that it entered `wasi:cli/run`.
///
/// The boot markers ride the serial line whatever transport the RPC
/// itself uses: they are printed before any RPC transport exists, and a
/// session on vsock still has to know when the guest is up.
pub(crate) async fn wait_for_boot(read: &mut RpcReader) -> Result<()> {
    runtime::timeout(boot_sync_timeout(), wait_for_debugger_stage(read))
        .await
        .context("timed out waiting for the embedded debugger cold-start markers")??;
    Ok(())
}

/// Keeps draining the guest's serial line for the rest of the session,
/// echoing what it carries.
///
/// With the RPC on vsock nothing else reads the serial socket, and a
/// socket QEMU cannot write into stops the guest console; this also
/// keeps the guest's own diagnostics visible, which is the whole reason
/// the console stays on the serial line.
pub(crate) fn echo_serial_console(mut read: RpcReader) {
    std::thread::spawn(move || {
        runtime::block_on(async move {
            let mut line = Vec::new();
            loop {
                match read_byte(&mut read).await {
                    Ok(Some(b'\n')) => {
                        if let Some(text) = printable_guest_line(&line) {
                            eprintln!("guest serial: {text}");
                        }
                        line.clear();
                    }
                    Ok(Some(b'\r')) => {}
                    Ok(Some(byte)) => line.push(byte),
                    Ok(None) | Err(_) => return,
                }
            }
        });
    });
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

async fn wait_for_debugger_stage(read: &mut RpcReader) -> Result<()> {
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
                        drain_boot_preamble(read).await?;
                        return Ok(());
                    }
                } else if let Some(text) = printable_guest_line(&line) {
                    eprintln!("guest serial: {text}");
                    if text.contains("panicked at") {
                        let trailer = collect_panic_trailer(read).await;
                        anyhow::bail!(
                            "kernel panicked before the embedded debugger entered \
                             wasi:cli/run: {text}{trailer}"
                        );
                    }
                }
                line.clear();
            }
            b'\r' => {}
            other => line.push(other),
        }
    }
}

/// Renders a non-marker serial line for diagnostics, or `None` when the
/// line is empty or carries no printable text (RPC framing bytes).
fn printable_guest_line(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let trimmed = text.trim_matches(|c: char| c.is_control() || c == '\u{fffd}');
    let printable = trimmed
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{fffd}')
        .count();
    (printable >= 4 && printable * 2 >= trimmed.chars().count()).then(|| trimmed.to_owned())
}

/// After a panic line appears, the guest usually follows with the panic
/// message and location on separate lines; gather them until the serial
/// link goes quiet so the failure carries the whole report.
async fn collect_panic_trailer(read: &mut RpcReader) -> String {
    let mut trailer = String::new();
    let mut line = Vec::new();
    loop {
        match runtime::timeout(Duration::from_secs(2), read_byte(read)).await {
            Some(Ok(Some(b'\n'))) => {
                if let Some(text) = printable_guest_line(&line) {
                    trailer.push_str("\n  ");
                    trailer.push_str(&text);
                }
                line.clear();
            }
            Some(Ok(Some(b'\r'))) => {}
            Some(Ok(Some(byte))) => line.push(byte),
            Some(Ok(None)) | Some(Err(_)) | None => break,
        }
    }
    if let Some(text) = printable_guest_line(&line) {
        trailer.push_str("\n  ");
        trailer.push_str(&text);
    }
    trailer
}

async fn drain_boot_preamble(read: &mut RpcReader) -> Result<()> {
    loop {
        match runtime::timeout(READY_DRAIN_QUIET_PERIOD, read_byte(read)).await {
            Some(Ok(Some(_))) => {}
            Some(Ok(None)) => {
                anyhow::bail!("debug serial link closed while draining boot preamble");
            }
            Some(Err(error)) => {
                return Err(error).context("failed to drain debugger boot preamble");
            }
            None => return Ok(()),
        }
    }
}

async fn read_byte(read: &mut RpcReader) -> std::io::Result<Option<u8>> {
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
