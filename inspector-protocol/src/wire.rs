use std::io;
use std::pin::Pin;

use futures_io::{AsyncRead, AsyncWrite};

#[cfg(feature = "host")]
use std::io::Write as _;

pub(crate) const FRAME_MAGIC: [u8; 8] = [0xff, 0x00, b'H', b'R', b'P', b'C', 0xaa, 0x55];
const MAX_FRAME_SIZE: usize = 1 << 20;

/// The line the kernel's panic path always writes to the console,
/// carrying the message and the source location on one line.
///
/// The frame scanner is the only host-side reader of the guest console
/// bytes that share the link with the frames, so it is the only place
/// that can turn a dead guest into an error instead of a wait.
#[cfg(feature = "host")]
const GUEST_PANIC_MARKER: &str = "Kernel panic:";

/// Console lines kept before the panic line so the report is whole.
///
/// A panic message may itself contain newlines, and the raw
/// `panicked at <file>:<line>` line precedes the kernel's own log
/// statement, so the line carrying the marker is the end of the report
/// and not all of it.
#[cfg(feature = "host")]
const GUEST_CONSOLE_LOOKBACK: usize = 3;

#[derive(Debug)]
pub(crate) enum Frame {
    Open {
        invocation: u32,
        instance: String,
        func: String,
    },
    Accept {
        invocation: u32,
    },
    Reject {
        invocation: u32,
        message: String,
    },
    Data {
        invocation: u32,
        path: Vec<u32>,
        payload: Vec<u8>,
    },
    Close {
        invocation: u32,
        path: Vec<u32>,
    },
}

pub(crate) async fn write_frame<T>(io: &mut T, frame: &Frame) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    let payload = encode_frame(frame)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame payload is too large"))?;
    let mut frame =
        Vec::with_capacity(FRAME_MAGIC.len() + core::mem::size_of::<u32>() + payload.len());
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    write_all(io, &frame).await?;
    flush(io).await
}

pub(crate) async fn read_frame<T>(io: &mut T) -> io::Result<Option<Frame>>
where
    T: AsyncRead + Unpin,
{
    if !sync_to_frame(io).await? {
        return Ok(None);
    }

    let mut len = [0_u8; 4];
    read_exact(io, &mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload {len} exceeds the maximum supported size"),
        ));
    }

    let mut payload = vec![0_u8; len];
    read_exact(io, &mut payload).await?;
    Ok(Some(decode_frame(&payload)?))
}

async fn sync_to_frame<T>(io: &mut T) -> io::Result<bool>
where
    T: AsyncRead + Unpin,
{
    let mut matched = 0usize;
    #[cfg(feature = "host")]
    let mut stray = Vec::new();
    #[cfg(feature = "host")]
    let mut console = GuestConsole::default();
    loop {
        let mut byte = [0_u8; 1];
        match std::future::poll_fn(|cx| Pin::new(&mut *io).poll_read(cx, &mut byte)).await {
            Ok(0) if matched == 0 => {
                #[cfg(feature = "host")]
                flush_stray_bytes(&mut stray, &mut console)?;
                return Ok(false);
            }
            Ok(0) => {
                #[cfg(feature = "host")]
                flush_stray_bytes(&mut stray, &mut console)?;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame header",
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(error),
        }

        if byte[0] == FRAME_MAGIC[matched] {
            matched += 1;
            if matched == FRAME_MAGIC.len() {
                #[cfg(feature = "host")]
                flush_stray_bytes(&mut stray, &mut console)?;
                return Ok(true);
            }
        } else {
            #[cfg(feature = "host")]
            {
                if matched != 0 {
                    stray.extend_from_slice(&FRAME_MAGIC[..matched]);
                }
                stray.push(byte[0]);
                flush_stray_bytes_on_newline(&mut stray, &mut console)?;
            }
            matched = usize::from(byte[0] == FRAME_MAGIC[0]);
        }
    }
}

/// The tail of the guest console this frame scan has seen.
///
/// The scanner is the only host-side reader of the console bytes that
/// share the link with the frames, so the context of a fatal line
/// exists here and nowhere else.
#[cfg(feature = "host")]
#[derive(Default)]
struct GuestConsole {
    recent: std::collections::VecDeque<String>,
}

#[cfg(feature = "host")]
impl GuestConsole {
    /// The panic report ending at `line`, or `None` while the guest is
    /// still alive. Remembers the line either way.
    fn read(&mut self, line: &str) -> Option<String> {
        let report = line.contains(GUEST_PANIC_MARKER).then(|| {
            self.recent
                .iter()
                .map(String::as_str)
                .chain(core::iter::once(line))
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" / ")
        });
        if self.recent.len() == GUEST_CONSOLE_LOOKBACK {
            self.recent.pop_front();
        }
        self.recent.push_back(line.to_owned());
        report
    }
}

/// One console line with the escape sequences the guest colours its log
/// with removed, which is the form a panic report is quoted in.
///
/// Dropping the escape character alone would leave the rest of each
/// sequence — `[1;31m` — in the middle of a report that ends up in a
/// JSON error field, so the whole sequence goes.
#[cfg(feature = "host")]
fn printable_console_line(line: &[u8]) -> String {
    let stripped = strip_ansi_escapes::strip(line);
    String::from_utf8_lossy(&stripped)
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(feature = "host")]
fn flush_stray_bytes_on_newline(stray: &mut Vec<u8>, console: &mut GuestConsole) -> io::Result<()> {
    if stray
        .last()
        .is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
    {
        flush_stray_bytes(stray, console)?;
    }
    Ok(())
}

#[cfg(feature = "host")]
fn flush_stray_bytes(stray: &mut Vec<u8>, console: &mut GuestConsole) -> io::Result<()> {
    if stray.is_empty() {
        return Ok(());
    }

    let report = console.read(&printable_console_line(stray));

    let mut stderr = std::io::stderr().lock();
    stderr.write_all(b"helios-inspector: remote serial: ")?;
    for &byte in stray.iter() {
        match byte {
            b'\n' => stderr.write_all(b"\\n")?,
            b'\r' => stderr.write_all(b"\\r")?,
            b'\t' => stderr.write_all(b"\\t")?,
            0x20..=0x7e => stderr.write_all(&[byte])?,
            other => {
                let encoded = [
                    b'\\',
                    b'x',
                    HEX[(other >> 4) as usize],
                    HEX[(other & 0x0f) as usize],
                ];
                stderr.write_all(&encoded)?;
            }
        }
    }
    stderr.write_all(b"\n")?;
    stderr.flush()?;
    stray.clear();
    drop(stderr);

    // The line is echoed before it is judged: a panic report is the last
    // thing the guest ever says, and it has to reach the log even though
    // it also ends the session.
    match report {
        Some(report) => Err(crate::error::TransportError::GuestPanicked { report }.into()),
        None => Ok(()),
    }
}

#[cfg(feature = "host")]
const HEX: &[u8; 16] = b"0123456789abcdef";

async fn write_all<T>(io: &mut T, mut bytes: &[u8]) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    while !bytes.is_empty() {
        let written = std::future::poll_fn(|cx| Pin::new(&mut *io).poll_write(cx, bytes)).await?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "async writer returned zero bytes",
            ));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

async fn flush<T>(io: &mut T) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *io).poll_flush(cx)).await
}

async fn read_exact<T>(io: &mut T, mut buffer: &mut [u8]) -> io::Result<()>
where
    T: AsyncRead + Unpin,
{
    while !buffer.is_empty() {
        let read = std::future::poll_fn(|cx| Pin::new(&mut *io).poll_read(cx, buffer)).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated frame payload",
            ));
        }
        let (_, rest) = buffer.split_at_mut(read);
        buffer = rest;
    }
    Ok(())
}

fn encode_frame(frame: &Frame) -> io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    match frame {
        Frame::Open {
            invocation,
            instance,
            func,
        } => {
            payload.push(0);
            encode_u32(&mut payload, *invocation);
            encode_string(&mut payload, instance)?;
            encode_string(&mut payload, func)?;
        }
        Frame::Accept { invocation } => {
            payload.push(1);
            encode_u32(&mut payload, *invocation);
        }
        Frame::Reject {
            invocation,
            message,
        } => {
            payload.push(2);
            encode_u32(&mut payload, *invocation);
            encode_string(&mut payload, message)?;
        }
        Frame::Data {
            invocation,
            path,
            payload: bytes,
        } => {
            payload.push(3);
            encode_u32(&mut payload, *invocation);
            encode_u32_vec(&mut payload, path)?;
            encode_bytes(&mut payload, bytes)?;
        }
        Frame::Close { invocation, path } => {
            payload.push(4);
            encode_u32(&mut payload, *invocation);
            encode_u32_vec(&mut payload, path)?;
        }
    }
    Ok(payload)
}

fn decode_frame(mut payload: &[u8]) -> io::Result<Frame> {
    let Some(tag) = payload.first().copied() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty frame payload",
        ));
    };
    payload = &payload[1..];
    match tag {
        0 => Ok(Frame::Open {
            invocation: decode_u32(&mut payload)?,
            instance: decode_string(&mut payload)?,
            func: decode_string(&mut payload)?,
        }),
        1 => Ok(Frame::Accept {
            invocation: decode_u32(&mut payload)?,
        }),
        2 => Ok(Frame::Reject {
            invocation: decode_u32(&mut payload)?,
            message: decode_string(&mut payload)?,
        }),
        3 => Ok(Frame::Data {
            invocation: decode_u32(&mut payload)?,
            path: decode_u32_vec(&mut payload)?,
            payload: decode_bytes(&mut payload)?,
        }),
        4 => Ok(Frame::Close {
            invocation: decode_u32(&mut payload)?,
            path: decode_u32_vec(&mut payload)?,
        }),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame tag {other}"),
        )),
    }
}

fn encode_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn decode_u32(payload: &mut &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated u32 field",
        ));
    }
    let (raw, rest) = payload.split_at(4);
    *payload = rest;
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(raw);
    Ok(u32::from_le_bytes(bytes))
}

fn encode_string(buffer: &mut Vec<u8>, value: &str) -> io::Result<()> {
    encode_bytes(buffer, value.as_bytes())
}

fn decode_string(payload: &mut &[u8]) -> io::Result<String> {
    String::from_utf8(decode_bytes(payload)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn encode_bytes(buffer: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "byte slice exceeds u32"))?;
    encode_u32(buffer, len);
    buffer.extend_from_slice(value);
    Ok(())
}

fn decode_bytes(payload: &mut &[u8]) -> io::Result<Vec<u8>> {
    let len = usize::try_from(decode_u32(payload)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated byte slice field",
        ));
    }
    let (bytes, rest) = payload.split_at(len);
    *payload = rest;
    Ok(bytes.to_vec())
}

fn encode_u32_vec(buffer: &mut Vec<u8>, values: &[u32]) -> io::Result<()> {
    let len = u32::try_from(values.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path exceeds u32 length"))?;
    encode_u32(buffer, len);
    for value in values {
        encode_u32(buffer, *value);
    }
    Ok(())
}

fn decode_u32_vec(payload: &mut &[u8]) -> io::Result<Vec<u32>> {
    let len = usize::try_from(decode_u32(payload)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut values = Vec::with_capacity(len);
    for _ in 0..len {
        values.push(decode_u32(payload)?);
    }
    Ok(values)
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use crate::error::TransportError;

    /// The four console lines the aarch64 bench lane's guest died on,
    /// colour codes and all, as the frame scanner sees them. The panic
    /// message itself contains a newline, so the kernel's own log
    /// statement is the fourth of them and carries only its first half.
    const PANIC_LINES: [&[u8]; 4] = [
        b"panicked at kernel/src/wasmtime_adapter/component_host/service/mod.rs:484:13:\n",
        b"failed to exec embedded system component:\n",
        b"debugger component trapped: instance terminated: OutOfMemory\n",
        b"\x1b[1;31mERROR\x1b[0m [helios_kernel] Kernel panic: failed to exec embedded system component:\n",
    ];

    fn console_report(lines: &[&[u8]]) -> Option<String> {
        let mut console = GuestConsole::default();
        let mut report = None;
        for line in lines {
            report = console.read(&printable_console_line(line)).or(report);
        }
        report
    }

    #[test]
    fn a_panic_report_carries_the_lines_that_led_to_it() {
        let report = console_report(&PANIC_LINES).expect("the console carries a panic report");
        assert_eq!(
            report,
            "panicked at kernel/src/wasmtime_adapter/component_host/service/mod.rs:484:13: / \
failed to exec embedded system component: / \
debugger component trapped: instance terminated: OutOfMemory / \
ERROR [helios_kernel] Kernel panic: failed to exec embedded system component:"
        );
    }

    #[test]
    fn the_look_back_does_not_reach_past_its_bound() {
        let mut lines = vec![b"INFO [helios_kernel] Kernel is ready\n".as_slice()];
        lines.extend_from_slice(&PANIC_LINES);
        let report = console_report(&lines).expect("the console carries a panic report");
        assert!(
            !report.contains("Kernel is ready"),
            "only the {GUEST_CONSOLE_LOOKBACK} lines before the panic are context: {report}"
        );
    }

    #[test]
    fn an_ordinary_console_line_is_not_a_panic_report() {
        assert!(console_report(&[b"INFO [helios_kernel] Kernel is ready\n"]).is_none());
    }

    #[test]
    fn a_flushed_panic_line_ends_the_session() {
        let mut console = GuestConsole::default();
        let mut error = None;
        for line in PANIC_LINES {
            let mut stray = line.to_vec();
            error = flush_stray_bytes(&mut stray, &mut console).err().or(error);
        }
        let error = error.expect("a panic must end the read");
        let transport = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<TransportError>())
            .expect("the error carries the typed transport fault");
        assert!(
            transport
                .guest_panic()
                .is_some_and(|report| report.contains("instance terminated: OutOfMemory")),
            "the fault names what the guest died of: {transport}"
        );
    }

    #[test]
    fn a_flushed_console_line_leaves_the_session_alone() {
        let mut console = GuestConsole::default();
        let mut stray = b"INFO [helios_kernel] Kernel is ready\n".to_vec();
        flush_stray_bytes(&mut stray, &mut console).expect("an ordinary line is not a fault");
        assert!(stray.is_empty());
    }
}
