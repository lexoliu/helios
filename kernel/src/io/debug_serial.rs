//! The debug UART, and the one owner every byte on it goes through.
//!
//! The machine has a single debug serial line and two byte streams
//! share it: the kernel console — tracing records, the embedded
//! debugger's `[KDBG …]` stage markers, the panic report — and the
//! inspector's RPC, whose frames are `magic || len || payload`. The
//! host reads the line by scanning for the magic and treating
//! everything else as console text, so a console record that lands
//! between a frame's magic and the end of its payload destroys that
//! frame: the reader has already committed to `len` bytes and takes
//! whatever is there (#103).
//!
//! What differs between machines is only how the port is reached. That
//! is [`DebugSerialAccess`], which a backend implements and nothing
//! else. Everything the kernel does with the bytes — who may write,
//! what is indivisible, what happens under contention — is kernel logic
//! and lives here, in [`DebugConsole`].
//!
//! # Concurrency contract
//!
//! - **One owner.** [`DebugConsole`] is the only thing that calls
//!   [`ByteSerial::write_bytes`] on the debug port. A backend supplies
//!   the accessor and no longer has a byte writer of its own. The one
//!   deliberate exception is [`PanicSerial`](super::PanicSerial): a
//!   panicking processor cannot wait for a port another processor may
//!   never release, so the panic report is written straight at the
//!   register, alloc-free and lock-free, and accepts that it may cut
//!   into whatever was on the wire. A panic ends the machine; a torn
//!   line before the report is a better outcome than no report.
//!
//! - **The unit of exclusion is a segment.** A segment is one complete
//!   console record (a tracing event, a stage marker, a kernel
//!   diagnostic), one complete guest write — which for the debugger is
//!   exactly one RPC frame, because its transport puts a frame on the
//!   wire with a single `serial.write` — or one line-sized piece of a
//!   guest byte stream. Two segments never interleave.
//!
//! - **Who may write, from which processor.** Any processor, in any
//!   context: a task, an interrupt handler, a bootstrap path that runs
//!   before the executor exists. The port itself is touched by exactly
//!   one processor at a time, the one holding the transmit role.
//!
//! - **Under contention.** The role is a single atomic flag, taken
//!   lock-free and held with interrupts *enabled* — a segment costs the
//!   device's own transmit time, and disabling interrupts across one is
//!   what #103 rejects. A kernel record that loses the race does not
//!   wait: [`DebugConsole::emit`] copies it into a buffer from a
//!   bounded kernel pool, pushes it on a lock-free queue and returns,
//!   and the processor holding the role writes it before releasing.
//!   That is what makes an interrupt handler safe on the very
//!   processor that is transmitting: it hands its record over and
//!   returns, instead of waiting for a transmit its own interrupt
//!   suspended.
//!
//! - **Guest bytes never queue.** [`DebugConsole::try_write`] takes the
//!   role or reports that the port is busy, and its callers are async
//!   host functions that `yield_now().await` and try again. The guest
//!   is therefore throttled by the device exactly as it was before, and
//!   cannot grow a kernel-owned queue by writing faster than the UART
//!   drains: kernel memory and user memory stay separate ownership
//!   domains.
//!
//! - **Order.** Segments leave in the order they were submitted: a
//!   segment never overtakes one that was already queued when it was
//!   handed over.

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::serial::ByteSerial;
use objectpool::{Pool, ReusableObject};
use spin::Once;

use super::serial::try_read_serial;

/// Bytes of a guest byte stream one segment carries at most.
///
/// A stream has no record structure of its own, so the console gives it
/// one: it cuts at the last newline inside this bound, and at the bound
/// itself when the piece holds no newline. A console record can then
/// land between two of the guest's lines but never inside one, and the
/// transmit role is never held for longer than this many bytes on the
/// stream path.
const MAX_STREAM_SEGMENT_BYTES: usize = 512;

/// Segments the hand-off queue holds for the processor that owns the
/// port.
///
/// Only kernel records reach it, and the kernel's own producer is the
/// tracing subscriber, whose log queue is a quarter of this deep. A
/// full queue is therefore a kernel invariant violation rather than a
/// load condition, and is reported the way the log queue reports its
/// own.
const HANDOFF_SEGMENTS: usize = 1024;

/// Buffers the hand-off pool keeps for reuse.
const HANDOFF_POOL_SLOTS: usize = 64;

/// Capacity a pooled segment buffer starts with, and the capacity above
/// which it is dropped rather than kept, so one long record does not
/// pin an oversized buffer for the rest of the boot.
const SEGMENT_BUFFER_INITIAL_CAPACITY: usize = 256;
const SEGMENT_BUFFER_RETAINED_CAPACITY: usize = 4096;

/// A backend's access to the machine's debug UART.
///
/// A backend implements this on the port type itself and supplies
/// nothing but the way to reach it, which is why the trait carries no
/// receiver: the port is a machine-wide device, not per-processor
/// state, and the kernel asks for it from wherever it happens to run.
///
/// Reaching a port that the backend has not brought up yet is a
/// programming error, not a condition to report: the transport that
/// would carry the report *is* the port. An implementation panics.
pub trait DebugSerialAccess {
    /// The port the backend reaches its debug UART through.
    type Port: ByteSerial;

    /// The machine's debug UART.
    fn port() -> Self::Port;
}

/// Drains what the debug UART has, up to `max_bytes`, into caller-owned
/// storage; leaves `buffer` empty when no byte is ready.
///
/// Monomorphised for one backend, this coerces to the [`SerialReader`]
/// the component host installs.
///
/// [`SerialReader`]: super::serial::SerialReader
pub fn read_debug_serial<Access: DebugSerialAccess>(buffer: &mut Vec<u8>, max_bytes: u32) {
    try_read_serial(&Access::port(), buffer, max_bytes);
}

/// The machine's one debug UART has one console.
///
/// The port is a machine-wide device reached through a receiver-less
/// accessor, so its owner is a machine-wide value too. It holds one
/// atomic flag and, once a processor has ever lost the race for the
/// port, the hand-off queue behind it.
static CONSOLE: DebugConsole = DebugConsole::new();

/// The kernel's writer for the machine's debug UART, monomorphised for
/// one backend.
///
/// This is what the component host, the guest-facing services and the
/// backends pass around instead of a raw byte writer, so there is no
/// way to reach the port that does not go through the console.
#[derive(Clone, Copy)]
pub struct DebugSerialWriter {
    emit: fn(&[u8]),
    try_write: fn(&[u8]) -> bool,
}

impl DebugSerialWriter {
    /// The writer for the backend that reaches the port through
    /// `Access`.
    #[must_use]
    pub const fn of<Access: DebugSerialAccess>() -> Self {
        Self {
            emit: emit_segment::<Access>,
            try_write: try_write_segment::<Access>,
        }
    }

    /// Hands one whole kernel record to the console.
    ///
    /// Never waits, so it is callable from an interrupt handler and
    /// from a bootstrap path that runs before the executor exists.
    pub fn emit(&self, record: &[u8]) {
        (self.emit)(record);
    }

    /// Hands one whole formatted kernel record to the console.
    ///
    /// `core::fmt` gives a sink one fragment per format piece, so the
    /// record is built in a pooled buffer first and reaches the port as
    /// a single segment.
    pub fn emit_fmt(&self, arguments: fmt::Arguments<'_>) {
        CONSOLE.with_segment_buffer(|buffer| {
            let mut record = SegmentWriter { buffer };
            record
                .write_fmt(arguments)
                .expect("a segment buffer never fails to take a formatted record");
            (self.emit)(buffer);
        });
    }

    /// Writes one whole guest segment, reporting `false` when another
    /// processor owns the port.
    ///
    /// The caller is an async host function and yields before trying
    /// again, so guest bytes are throttled by the device rather than
    /// buffered in kernel memory.
    #[must_use]
    pub fn try_write(&self, segment: &[u8]) -> bool {
        (self.try_write)(segment)
    }

    /// Writes as much of a guest byte stream as the console will take
    /// now, and reports how many bytes that was.
    ///
    /// Zero means another processor owns the port and the caller has to
    /// yield and try again. A short count means the stream was cut at a
    /// line boundary, which is the granularity at which a console
    /// record may reach the wire between the guest's own lines.
    #[must_use]
    pub fn write_stream(&self, bytes: &[u8]) -> usize {
        let segment = stream_segment(bytes);
        if segment.is_empty() || self.try_write(segment) {
            segment.len()
        } else {
            0
        }
    }

    /// Writes one `[KDBG …]` stage marker as a single segment.
    ///
    /// The marker owns its line — it opens and closes with a newline —
    /// and the inspector's boot reader parses a whole line as one
    /// marker, so it has to reach the wire indivisibly.
    pub fn emit_stage_marker(&self, stage: &str) {
        self.emit_fmt(format_args!("\n[KDBG {stage}]\n"));
    }

    /// Writes one `[KDBG <label>: <message>]` error marker as a single
    /// segment, with the characters that would end the marker early
    /// replaced.
    pub fn emit_error_marker(&self, label: &str, message: impl fmt::Display) {
        self.emit_fmt(format_args!(
            "\n[KDBG {label}: {message}]\n",
            message = MarkerText(message)
        ));
    }
}

/// One console message with the characters that would end a marker
/// early replaced: a newline would close its line and a `]` would close
/// the marker itself.
struct MarkerText<Message>(Message);

impl<Message: fmt::Display> fmt::Display for MarkerText<Message> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(MarkerEscape { formatter }, "{}", self.0)
    }
}

/// Escapes as it goes, so the message never has to be gathered into a
/// buffer of its own before it is rendered into the marker's.
struct MarkerEscape<'a, 'b> {
    formatter: &'a mut fmt::Formatter<'b>,
}

impl Write for MarkerEscape<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            let rendered = match character {
                '\n' | '\r' => ' ',
                ']' => ')',
                other => other,
            };
            self.formatter.write_char(rendered)?;
        }
        Ok(())
    }
}

fn emit_segment<Access: DebugSerialAccess>(record: &[u8]) {
    CONSOLE.emit(&Access::port(), record);
}

fn try_write_segment<Access: DebugSerialAccess>(segment: &[u8]) -> bool {
    CONSOLE.try_write(&Access::port(), segment)
}

/// The piece of a guest byte stream the console takes as one segment.
fn stream_segment(bytes: &[u8]) -> &[u8] {
    if bytes.len() <= MAX_STREAM_SEGMENT_BYTES {
        return bytes;
    }
    let head = &bytes[..MAX_STREAM_SEGMENT_BYTES];
    match head.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => &head[..=end],
        None => head,
    }
}

/// The owner of one debug UART.
///
/// The port is passed in rather than held, because the backend's
/// accessor is receiver-less and because a test drives this over its
/// own recording sink. What the console owns is not the value but the
/// right to write to it: no other code path calls `write_bytes` on the
/// debug port.
pub struct DebugConsole {
    /// Set by the processor that owns the port right now.
    transmitting: AtomicBool,
    /// Built the first time a processor loses the race for the port; a
    /// machine whose console is never contended never allocates it.
    handoff: Once<Handoff>,
}

struct Handoff {
    queue: ConcurrentQueue<ReusableObject<Vec<u8>>>,
    buffers: Pool<Vec<u8>>,
}

impl Handoff {
    fn new() -> Self {
        Self {
            queue: ConcurrentQueue::bounded(HANDOFF_SEGMENTS),
            buffers: Pool::bounded(HANDOFF_POOL_SLOTS, new_segment_buffer, reset_segment_buffer),
        }
    }
}

impl Default for DebugConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugConsole {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transmitting: AtomicBool::new(false),
            handoff: Once::new(),
        }
    }

    /// Hands one whole kernel record to the port without ever waiting.
    pub fn emit(&self, port: &impl ByteSerial, record: &[u8]) {
        if self.take_port() {
            // Anything queued before this record goes first, so a
            // record never overtakes one that was already waiting.
            self.drain(port);
            port.write_bytes(record);
            self.drain(port);
            self.release_port();
        } else {
            self.hand_over(record);
        }
        self.pump(port);
    }

    /// Writes one whole guest segment, or reports that the port is
    /// busy.
    #[must_use]
    pub fn try_write(&self, port: &impl ByteSerial, segment: &[u8]) -> bool {
        if !self.take_port() {
            return false;
        }
        self.drain(port);
        port.write_bytes(segment);
        self.drain(port);
        self.release_port();
        self.pump(port);
        true
    }

    /// Runs `build` over a pooled segment buffer.
    fn with_segment_buffer<Built>(&self, build: impl FnOnce(&mut Vec<u8>) -> Built) -> Built {
        let mut buffer = self.handoff().buffers.get_owned();
        build(&mut buffer)
    }

    fn handoff(&self) -> &Handoff {
        self.handoff.call_once(Handoff::new)
    }

    fn take_port(&self) -> bool {
        self.transmitting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release_port(&self) {
        self.transmitting.store(false, Ordering::Release);
    }

    fn hand_over(&self, record: &[u8]) {
        let handoff = self.handoff();
        let mut buffer = handoff.buffers.get_owned();
        buffer.extend_from_slice(record);
        match handoff.queue.push(buffer) {
            Ok(()) => {}
            Err(PushError::Full(_)) => panic!(
                "debug console hand-off capacity {HANDOFF_SEGMENTS} exhausted while another processor owned the port"
            ),
            Err(PushError::Closed(_)) => {
                panic!("the debug console hand-off queue was closed unexpectedly")
            }
        }
    }

    /// Takes the port and writes whatever is queued, until the queue is
    /// empty or another processor owns the port.
    ///
    /// This is what closes the race between a processor that queued a
    /// segment and one that released the port a moment earlier without
    /// having seen it.
    fn pump(&self, port: &impl ByteSerial) {
        loop {
            if self.handoff_is_empty() {
                return;
            }
            if !self.take_port() {
                // Whoever owns the port drains what is queued before it
                // releases, and comes back through here afterwards.
                return;
            }
            self.drain(port);
            self.release_port();
        }
    }

    fn handoff_is_empty(&self) -> bool {
        self.handoff
            .get()
            .is_none_or(|handoff| handoff.queue.is_empty())
    }

    /// Writes every queued segment. The caller owns the port.
    fn drain(&self, port: &impl ByteSerial) {
        let Some(handoff) = self.handoff.get() else {
            return;
        };
        loop {
            match handoff.queue.pop() {
                Ok(segment) => port.write_bytes(&segment),
                Err(PopError::Empty | PopError::Closed) => return,
            }
        }
    }
}

/// A `core::fmt` sink that gathers a whole record before it is emitted.
struct SegmentWriter<'a> {
    buffer: &'a mut Vec<u8>,
}

impl Write for SegmentWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.buffer.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

fn new_segment_buffer() -> Vec<u8> {
    Vec::with_capacity(SEGMENT_BUFFER_INITIAL_CAPACITY)
}

fn reset_segment_buffer(buffer: &mut Vec<u8>) {
    if buffer.capacity() > SEGMENT_BUFFER_RETAINED_CAPACITY {
        *buffer = new_segment_buffer();
    } else {
        buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use helios_hal::serial::ByteSerial;

    use super::{DebugConsole, MAX_STREAM_SEGMENT_BYTES, stream_segment};

    /// The RPC frame header the inspector scans for, byte for byte as
    /// `helios-inspector-protocol` writes it. The point of the test is
    /// that a frame reaches the host whole, so it is built here the way
    /// the wire carries it rather than described.
    const FRAME_MAGIC: [u8; 8] = [0xff, 0x00, b'H', b'R', b'P', b'C', 0xaa, 0x55];
    const FRAME_PAYLOAD_BYTES: usize = 64;
    const FRAMES_PER_WRITER: usize = 64;
    const RECORDS_PER_WRITER: usize = 256;
    const CONSOLE_RECORD: &[u8] = b"INFO [helios_kernel::memory::balloon] reporting started\n";

    /// A sink shaped like the UART the backends write to: one byte at a
    /// time, with a yield between bytes, so a writer that did not own
    /// the port is split by whoever does.
    #[derive(Default)]
    struct RecordingSink {
        bytes: Mutex<Vec<u8>>,
    }

    impl RecordingSink {
        fn taken(&self) -> Vec<u8> {
            self.bytes
                .lock()
                .expect("the recording sink was poisoned")
                .clone()
        }
    }

    impl ByteSerial for RecordingSink {
        fn try_read_byte(&self) -> Option<u8> {
            None
        }

        fn write_bytes(&self, bytes: &[u8]) {
            for &byte in bytes {
                self.bytes
                    .lock()
                    .expect("the recording sink was poisoned")
                    .push(byte);
                thread::yield_now();
            }
        }
    }

    /// One RPC frame as the guest's transport hands it over: the magic,
    /// the little-endian payload length, then the payload.
    fn frame(writer: u8, index: usize) -> Vec<u8> {
        let payload: Vec<u8> = (0..FRAME_PAYLOAD_BYTES)
            .map(|offset| (offset as u8) ^ writer)
            .collect();
        let mut frame = Vec::from(FRAME_MAGIC);
        frame.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("the test payload fits a u32")
                .to_le_bytes(),
        );
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&index.to_le_bytes());
        frame
    }

    /// Frames the sink's bytes the way the host's `sync_to_frame` does:
    /// scan for the magic, take the length, take that many bytes.
    fn frames_on_the_wire(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset + FRAME_MAGIC.len() + 4 <= bytes.len() {
            if bytes[offset..offset + FRAME_MAGIC.len()] != FRAME_MAGIC {
                offset += 1;
                continue;
            }
            let header = offset + FRAME_MAGIC.len();
            let mut length = [0_u8; 4];
            length.copy_from_slice(&bytes[header..header + 4]);
            let length = u32::from_le_bytes(length) as usize;
            assert_eq!(
                length, FRAME_PAYLOAD_BYTES,
                "a console record landed inside a frame header"
            );
            let end = header + 4 + length + size_of::<usize>();
            assert!(end <= bytes.len(), "a frame was cut short on the wire");
            frames.push(bytes[offset..end].to_vec());
            offset = end;
        }
        frames
    }

    /// Regression for #103: a console record emitted by one processor
    /// must not land inside an RPC frame another is writing.
    ///
    /// The writers are the two real ones — the kernel console, which
    /// hands its record over rather than waiting, and the debugger's
    /// transport, which retries until it owns the port — over a sink
    /// that yields between bytes, so every byte is an interleaving
    /// point.
    #[test]
    fn a_frame_and_a_console_record_never_interleave() {
        let console = Arc::new(DebugConsole::new());
        let sink = Arc::new(RecordingSink::default());

        let writers: Vec<_> = [1_u8, 2]
            .into_iter()
            .map(|writer| {
                let console = console.clone();
                let sink = sink.clone();
                thread::spawn(move || {
                    for index in 0..FRAMES_PER_WRITER {
                        let frame = frame(writer, index);
                        while !console.try_write(&*sink, &frame) {
                            thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        let records = {
            let console = console.clone();
            let sink = sink.clone();
            thread::spawn(move || {
                for _ in 0..RECORDS_PER_WRITER {
                    console.emit(&*sink, CONSOLE_RECORD);
                    thread::yield_now();
                }
            })
        };
        for writer in writers {
            writer.join().expect("a frame writer panicked");
        }
        records.join().expect("the console record writer panicked");
        // Nothing is left behind in the hand-off queue.
        console.emit(&*sink, b"");

        let taken = sink.taken();
        let framed = frames_on_the_wire(&taken);
        assert_eq!(framed.len(), FRAMES_PER_WRITER * 2);
        for writer in [1_u8, 2] {
            for index in 0..FRAMES_PER_WRITER {
                assert!(
                    framed.contains(&frame(writer, index)),
                    "frame {index} from writer {writer} did not reach the wire whole"
                );
            }
        }

        let records = taken
            .windows(CONSOLE_RECORD.len())
            .filter(|window| *window == CONSOLE_RECORD)
            .count();
        assert_eq!(
            records, RECORDS_PER_WRITER,
            "a console record was split by a frame"
        );
    }

    #[test]
    fn a_short_stream_is_one_segment() {
        assert_eq!(stream_segment(b"hello\n"), b"hello\n");
    }

    #[test]
    fn a_long_stream_is_cut_at_its_last_line_within_the_bound() {
        let mut bytes = vec![b'a'; MAX_STREAM_SEGMENT_BYTES - 4];
        bytes.push(b'\n');
        bytes.extend_from_slice(&[b'b'; 16]);
        let segment = stream_segment(&bytes);
        assert_eq!(segment.len(), MAX_STREAM_SEGMENT_BYTES - 3);
        assert_eq!(segment.last(), Some(&b'\n'));
    }

    #[test]
    fn a_long_stream_without_a_line_is_cut_at_the_bound() {
        let bytes = vec![b'a'; MAX_STREAM_SEGMENT_BYTES * 2];
        assert_eq!(stream_segment(&bytes).len(), MAX_STREAM_SEGMENT_BYTES);
    }
}
