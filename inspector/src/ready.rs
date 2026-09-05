use std::io;
use std::pin::Pin;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_io::AsyncRead;
use helios_inspector_protocol::system::stats;

use crate::runtime;
use crate::serial::{RpcClient, RpcReader, SerialIo};

const BOOT_SYNC_TIMEOUT: Duration = Duration::from_secs(900);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const READY_DRAIN_QUIET_PERIOD: Duration = Duration::from_millis(100);
const PANIC_TRAILER_QUIET_PERIOD: Duration = Duration::from_secs(2);
const DEBUGGER_RUN_STAGE: &str = "run:begin";
/// Bytes one serial line may gather before it is rendered anyway.
///
/// A guest that stops emitting newlines — a corrupted stream, a binary
/// blob on the console — would otherwise grow a single line for as long
/// as the session runs, and an unbounded line is also what a log viewer
/// refuses to render.
const MAX_GUEST_LINE_BYTES: usize = 8 * 1024;

/// Bytes one read of the debug serial line takes at once.
///
/// The line is drained in whole chunks and never a byte per wakeup.
/// QEMU's 16550 model hands the guest's byte to the chardev, and when
/// the host socket is not writable it re-arms a writability watch a
/// bounded number of times (`MAX_XMIT_RETRY`) before discarding the
/// byte outright, so a host that reads slowly costs guest *output*,
/// not just latency. The chunk covers the largest burst the boot log
/// produces between wakeups in a single syscall.
const SERIAL_READ_CHUNK_BYTES: usize = 64 * 1024;

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
    let (read, write) = io.into_split();
    let read = wait_for_boot(read).await?;
    let mut client = helios_inspector_protocol::transport::Client::new(read, write);
    wait_until_ready(&mut client).await?;
    Ok(client)
}

/// Waits on the guest's serial line until the embedded debugger reports
/// that it entered `wasi:cli/run`, and hands the transport back.
///
/// The boot markers ride the serial line whatever transport the RPC
/// itself uses: they are printed before any RPC transport exists, and a
/// session on vsock still has to know when the guest is up.
///
/// The transport comes back drained: the caller's next reader — the RPC
/// client or the console echo — starts on the byte after the preamble.
pub(crate) async fn wait_for_boot(read: RpcReader) -> Result<RpcReader> {
    let echo = ConsoleEcho::new();
    let mut lines = SerialLines::new(read);
    runtime::timeout(
        boot_sync_timeout(),
        wait_for_debugger_stage(&mut lines, &echo),
    )
    .await
    .context("timed out waiting for the embedded debugger cold-start markers")??;
    Ok(lines.into_transport())
}

/// Keeps draining the guest's serial line for the rest of the session,
/// echoing what it carries.
///
/// With the RPC on vsock nothing else reads the serial socket, and a
/// socket QEMU cannot write into stops the guest console; this also
/// keeps the guest's own diagnostics visible, which is the whole reason
/// the console stays on the serial line.
pub(crate) fn echo_serial_console(read: RpcReader) {
    std::thread::spawn(move || {
        runtime::block_on(async move {
            let echo = ConsoleEcho::new();
            let mut lines = SerialLines::new(read);
            loop {
                match lines.advance().await {
                    Ok(true) => {
                        if let Some(text) = printable_guest_line(lines.line()) {
                            echo.guest_line(&text);
                        }
                    }
                    Ok(false) | Err(_) => return,
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

/// The debug serial line, framed into the lines the guest wrote.
///
/// Reading is the only thing that happens between one `poll_read` and
/// the next: framing scans bytes that have already been taken, and
/// rendering a line happens on [`ConsoleEcho`]'s thread. Nothing that
/// can block sits on this path, because a debug serial that stops being
/// drained does not queue — QEMU discards the guest's bytes, which is
/// how x86 stage markers lost characters in the middle of AP bring-up
/// (issue #102).
struct SerialLines<R> {
    read: R,
    /// The bytes of the last read, `taken` of which are already framed.
    chunk: Vec<u8>,
    filled: usize,
    taken: usize,
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> SerialLines<R> {
    fn new(read: R) -> Self {
        Self {
            read,
            chunk: vec![0; SERIAL_READ_CHUNK_BYTES],
            filled: 0,
            taken: 0,
            line: Vec::new(),
        }
    }

    /// The line [`Self::advance`] last framed.
    ///
    /// After `advance` reported end of stream, or after its future was
    /// dropped, this holds whatever of a line the guest had written —
    /// the tail of a console that stopped mid-sentence is evidence too.
    fn line(&self) -> &[u8] {
        &self.line
    }

    /// Gives the transport back, once nothing read from it is still
    /// held here.
    fn into_transport(self) -> R {
        debug_assert_eq!(
            self.taken, self.filled,
            "the debug serial transport was handed on with bytes still buffered"
        );
        self.read
    }

    /// Frames the bytes of the current chunk that are not framed yet,
    /// reporting whether that completed a line.
    fn frame_buffered(&mut self) -> bool {
        while self.taken < self.filled {
            let byte = self.chunk[self.taken];
            self.taken += 1;
            match byte {
                b'\n' => return true,
                b'\r' => {}
                other => {
                    self.line.push(other);
                    if self.line.len() >= MAX_GUEST_LINE_BYTES {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Takes the next chunk the transport holds, reporting its length;
    /// zero is end of stream.
    async fn fill(&mut self) -> io::Result<usize> {
        debug_assert_eq!(
            self.taken, self.filled,
            "a debug serial chunk was refilled over unframed bytes"
        );
        let read = &mut self.read;
        let chunk = &mut self.chunk;
        let count =
            std::future::poll_fn(|cx| Pin::new(&mut *read).poll_read(cx, chunk.as_mut_slice()))
                .await?;
        self.filled = count;
        self.taken = 0;
        Ok(count)
    }

    /// Frames the next line, reading as much as the transport holds
    /// whenever it has to read. `false` is end of stream.
    async fn advance(&mut self) -> io::Result<bool> {
        self.line.clear();
        loop {
            if self.frame_buffered() {
                return Ok(true);
            }
            if self.fill().await? == 0 {
                return Ok(false);
            }
        }
    }

    /// Discards whatever the guest is still writing until the line goes
    /// quiet for `quiet`, so the transport can be handed on.
    ///
    /// `false` means the transport closed while draining.
    async fn drain_until_quiet(&mut self, quiet: Duration) -> io::Result<bool> {
        loop {
            self.taken = self.filled;
            self.line.clear();
            match runtime::timeout(quiet, self.fill()).await {
                Some(Ok(0)) => return Ok(false),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error),
                None => return Ok(true),
            }
        }
    }
}

async fn wait_for_debugger_stage(
    lines: &mut SerialLines<RpcReader>,
    echo: &ConsoleEcho,
) -> Result<()> {
    loop {
        let framed = lines
            .advance()
            .await
            .context("failed to read kernel boot markers from the debug serial link")?;
        if !framed {
            anyhow::bail!(
                "debug serial link closed before the embedded debugger entered wasi:cli/run"
            );
        }

        if let Some(stage) = parse_stage_marker(lines.line())? {
            let run_begin = stage == DEBUGGER_RUN_STAGE;
            echo.stage(stage);
            if run_begin {
                drain_boot_preamble(lines).await?;
                return Ok(());
            }
        } else if let Some(text) = printable_guest_line(lines.line()) {
            echo.guest_line(&text);
            if text.contains("panicked at") {
                let trailer = collect_panic_trailer(lines).await;
                anyhow::bail!(
                    "kernel panicked before the embedded debugger entered \
                     wasi:cli/run: {text}{trailer}"
                );
            }
        }
    }
}

/// The one control character a rendered line keeps.
///
/// The guest colours its log with ANSI escape sequences, which read
/// correctly in a terminal and in a CI log alike.
const ESCAPE: char = '\u{1b}';

/// Renders a non-marker serial line for diagnostics, or `None` when the
/// line is empty or carries no printable text (RPC framing bytes).
///
/// Control characters are dropped from the middle of the line and not
/// only trimmed off its ends. A serial line that carries RPC framing or
/// a partially written buffer can hold a NUL anywhere in it, and one NUL
/// is enough for a CI log collector to discard the whole step's output —
/// which is exactly the evidence a failing boot exists to leave behind.
fn printable_guest_line(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let trimmed = text.trim_matches(|c: char| c.is_control() || c == '\u{fffd}');
    let printable = trimmed
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{fffd}')
        .count();
    if printable < 4 || printable * 2 < trimmed.chars().count() {
        return None;
    }
    Some(
        trimmed
            .chars()
            .filter(|character| *character == ESCAPE || !character.is_control())
            .collect(),
    )
}

/// After a panic line appears, the guest usually follows with the panic
/// message and location on separate lines; gather them until the serial
/// link goes quiet so the failure carries the whole report.
async fn collect_panic_trailer(lines: &mut SerialLines<RpcReader>) -> String {
    let mut trailer = String::new();
    while let Some(Ok(true)) = runtime::timeout(PANIC_TRAILER_QUIET_PERIOD, lines.advance()).await {
        if let Some(text) = printable_guest_line(lines.line()) {
            trailer.push_str("\n  ");
            trailer.push_str(&text);
        }
    }
    if let Some(text) = printable_guest_line(lines.line()) {
        trailer.push_str("\n  ");
        trailer.push_str(&text);
    }
    trailer
}

async fn drain_boot_preamble(lines: &mut SerialLines<RpcReader>) -> Result<()> {
    let open = lines
        .drain_until_quiet(READY_DRAIN_QUIET_PERIOD)
        .await
        .context("failed to drain debugger boot preamble")?;
    if !open {
        anyhow::bail!("debug serial link closed while draining boot preamble");
    }
    Ok(())
}

/// How a `[KDBG …]` stage marker opens.
const MARKER_PREFIX: &str = "[KDBG ";

/// The stage a marker line names, or `None` when the line carries none.
///
/// A marker owns its line. The kernel writes it as `\n[KDBG <stage>]\n`
/// in one segment through the single owner of the debug UART, so the
/// leading newline closes whatever preceded it and nothing else reaches
/// the wire before the trailing one (#103). This therefore reads the
/// whole line as one marker instead of hunting for the prefix inside a
/// line that should not have held anything else; a line that carries
/// the prefix and is not a marker is that guarantee breaking, and says
/// so rather than recovering a marker out of it.
fn parse_stage_marker(line: &[u8]) -> Result<Option<&str>> {
    let text =
        std::str::from_utf8(line).context("debug serial preamble contained non-utf8 bytes")?;
    if !text.contains(MARKER_PREFIX) {
        return Ok(None);
    }
    let Some(stage) = text
        .strip_prefix(MARKER_PREFIX)
        .and_then(|stage| stage.strip_suffix(']'))
    else {
        anyhow::bail!("a debugger stage marker shared its serial line with other output: {text:?}");
    };
    if stage.contains(']') {
        anyhow::bail!("multiple debugger stage markers appeared on one serial line: {text:?}");
    }
    Ok(Some(stage))
}

fn stage_detail(stage: &str) -> &str {
    match stage {
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
        other => other,
    }
}

/// The inspector's rendering of the guest console, on its own thread.
///
/// Whatever drains the debug serial must not write to stderr itself.
/// The inspector's own output is a pipe in every CI lane, and a write
/// that waits on the far end of that pipe is time the serial socket is
/// not being read — which costs guest bytes, not just ordering. Lines
/// are queued here and rendered in the order they were framed.
struct ConsoleEcho {
    lines: Option<mpsc::Sender<String>>,
    render: Option<JoinHandle<()>>,
}

impl ConsoleEcho {
    fn new() -> Self {
        let (lines, queued) = mpsc::channel::<String>();
        let render = std::thread::spawn(move || {
            for line in queued {
                eprintln!("{line}");
            }
        });
        Self {
            lines: Some(lines),
            render: Some(render),
        }
    }

    fn guest_line(&self, text: &str) {
        self.render(format!("guest serial: {text}"));
    }

    fn stage(&self, stage: &str) {
        self.render(format!("helios-inspector: {}", stage_detail(stage)));
    }

    fn render(&self, line: String) {
        let Some(lines) = &self.lines else {
            unreachable!("the console echo queue outlives every line sent through it");
        };
        if lines.send(line).is_err() {
            unreachable!("the console echo renderer outlives the queue it drains");
        }
    }
}

impl Drop for ConsoleEcho {
    fn drop(&mut self) {
        // Closing the queue is what ends the renderer's loop, so it has
        // to happen before the join, and the join is what keeps the
        // last framed lines from disappearing with the session.
        drop(self.lines.take());
        let Some(render) = self.render.take() else {
            unreachable!("the console echo renderer is joined exactly once");
        };
        if render.join().is_err() {
            panic!("the console echo renderer panicked while rendering the guest console");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    /// A transport that hands out a byte stream in a fixed rhythm of
    /// chunk sizes, so a marker is split at a different offset in every
    /// run.
    ///
    /// Every `stall_every`-th poll yields first, which is what a socket
    /// with nothing buffered yet does to the reader.
    struct ChunkedTransport {
        bytes: Vec<u8>,
        position: usize,
        chunks: Vec<usize>,
        polls: usize,
        stall_every: usize,
    }

    impl ChunkedTransport {
        fn new(bytes: &[u8], chunks: &[usize], stall_every: usize) -> Self {
            assert!(chunks.iter().all(|chunk| *chunk > 0));
            Self {
                bytes: bytes.to_vec(),
                position: 0,
                chunks: chunks.to_vec(),
                polls: 0,
                stall_every,
            }
        }
    }

    impl AsyncRead for ChunkedTransport {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.polls += 1;
            if this.stall_every != 0 && this.polls.is_multiple_of(this.stall_every) {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let chunk = this.chunks[this.polls % this.chunks.len()];
            let count = chunk.min(buf.len()).min(this.bytes.len() - this.position);
            buf[..count].copy_from_slice(&this.bytes[this.position..this.position + count]);
            this.position += count;
            Poll::Ready(Ok(count))
        }
    }

    /// A boot preamble whose markers sit among console output, the way
    /// the x86 console mirror interleaves them.
    const BOOT_LOG: &str = "\
[KDBG boot]
 INFO helios_kernel: processor 1 online
 INFO helios_kernel: processor 2 online
[KDBG engine:new]
 INFO helios_kernel::wasmtime_adapter: creating system component engine
[KDBG engine:ok]
[KDBG component:ok]
 INFO helios_kernel: virtio-net online queues=4
[KDBG run:begin]
";

    fn frame_all(bytes: &[u8], chunks: &[usize], stall_every: usize) -> Vec<String> {
        let transport = ChunkedTransport::new(bytes, chunks, stall_every);
        let mut lines = SerialLines::new(transport);
        runtime::block_on(async move {
            let mut framed = Vec::new();
            while lines
                .advance()
                .await
                .expect("chunked transport never fails")
            {
                framed.push(String::from_utf8(lines.line().to_vec()).expect("utf-8 line"));
            }
            framed
        })
    }

    fn stages(framed: &[String]) -> Vec<String> {
        framed
            .iter()
            .filter_map(|line| {
                parse_stage_marker(line.as_bytes())
                    .expect("a framed line carries at most one whole marker")
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn a_line_is_framed_the_same_whatever_chunks_it_arrives_in() {
        let expected: Vec<String> = BOOT_LOG.lines().map(str::to_owned).collect();
        for chunk in 1..=BOOT_LOG.len() {
            let framed = frame_all(BOOT_LOG.as_bytes(), &[chunk], 0);
            assert_eq!(framed, expected, "chunk size {chunk} framed differently");
        }
    }

    #[test]
    fn a_stage_marker_split_across_reads_still_parses_whole() {
        let expected = vec![
            "boot".to_owned(),
            "engine:new".to_owned(),
            "engine:ok".to_owned(),
            "component:ok".to_owned(),
            "run:begin".to_owned(),
        ];
        // An uneven rhythm splits every marker at a different offset,
        // and the stall makes the reader come back to a half-read line.
        for chunks in [
            vec![1_usize],
            vec![2, 3, 5, 7],
            vec![11, 1, 4],
            vec![BOOT_LOG.len()],
        ] {
            for stall_every in [0_usize, 2, 3] {
                let framed = frame_all(BOOT_LOG.as_bytes(), &chunks, stall_every);
                assert_eq!(
                    stages(&framed),
                    expected,
                    "chunks {chunks:?} stalling every {stall_every} lost a marker"
                );
            }
        }
    }

    #[test]
    fn a_marker_owns_its_line() {
        assert_eq!(
            parse_stage_marker(b"[KDBG run:begin]").expect("a whole marker parses"),
            Some("run:begin")
        );
        assert_eq!(
            parse_stage_marker(b" INFO helios_kernel: processor 1 online")
                .expect("an ordinary console line is not a marker"),
            None
        );
    }

    /// A marker that shares its line is the single-owner guarantee
    /// breaking, so it is reported rather than recovered from.
    #[test]
    fn a_marker_that_shares_its_line_is_a_fault() {
        for line in [
            b"INFO helios_kernel: online[KDBG boot]".as_slice(),
            b"[KDBG boot][KDBG engine:new]".as_slice(),
            b"[KDBG boot".as_slice(),
        ] {
            let error = parse_stage_marker(line)
                .expect_err("a marker that does not own its line is a fault");
            assert!(
                error.to_string().contains("serial line"),
                "the fault names the line it read: {error}"
            );
        }
    }

    #[test]
    fn a_carriage_return_never_reaches_a_framed_line() {
        let framed = frame_all(b"[KDBG boot]\r\nplain line\r\n", &[1, 6], 0);
        assert_eq!(framed, vec!["[KDBG boot]", "plain line"]);
    }

    #[test]
    fn a_line_that_never_ends_is_framed_at_the_cap() {
        let mut stream = "x".repeat(MAX_GUEST_LINE_BYTES + 16);
        stream.push('\n');
        let framed = frame_all(stream.as_bytes(), &[7], 0);
        assert_eq!(framed.len(), 2);
        assert_eq!(framed[0].len(), MAX_GUEST_LINE_BYTES);
        assert_eq!(framed[1].len(), 16);
    }

    #[test]
    fn the_tail_the_guest_left_unterminated_stays_readable() {
        let transport = ChunkedTransport::new(b"[KDBG boot]\npanicked at", &[3], 0);
        let mut lines = SerialLines::new(transport);
        runtime::block_on(async move {
            assert!(lines.advance().await.expect("framing the first line"));
            assert_eq!(lines.line(), b"[KDBG boot]");
            assert!(!lines.advance().await.expect("reaching end of stream"));
            assert_eq!(lines.line(), b"panicked at");
        });
    }
}
