extern crate alloc;

use alloc::string::String;
use core::fmt::{self, Write};
use core::mem;

use super::debug_serial::DebugSerialWriter;
use crate::ComponentRuntimeState;

/// Capacity a console's record buffer starts with, and the capacity
/// above which it is released rather than kept, so one long record does
/// not pin an oversized buffer for the rest of the boot.
const RECORD_BUFFER_INITIAL_CAPACITY: usize = 256;
const RECORD_BUFFER_RETAINED_CAPACITY: usize = 4096;

/// The kernel console: it keeps every record for the debugger and puts
/// the same record on the machine's debug UART.
///
/// Backends instantiate this with their own tick source, and with the
/// [`DebugSerialWriter`] of the port they brought up — or with `None`
/// when the machine's debug line is carrying something else and the
/// console is a debugger-only record.
///
/// # Why the mirror is a `DebugSerialWriter` and not a byte sink
///
/// The kernel console is not the only producer on that UART: the
/// embedded debugger's `[KDBG …]` stage markers and the inspector's RPC
/// frames go out on the same wire. [`DebugConsole`](super::DebugConsole)
/// exists so that all of them are ordered by one transmit role, and a
/// record handed to it reaches the port whole whoever else is writing.
///
/// A console that took an arbitrary `FnMut(&[u8])` let a backend put a
/// second writer on that wire beside the console rather than through
/// it, and riscv did: it mirrored kernel tracing to the SBI console
/// byte by byte while the markers went to the same 16550 through the
/// [`DebugConsole`]. Two writers, two disciplines, one device — so a
/// marker emitted while the balloon's periodic report was mid-write came
/// out spliced into it as `"emory balloo[KDBG boot]"` (#164). Naming
/// the writer in the type is what stops that from being expressible.
///
/// # The record is the segment
///
/// The unit the console promises to deliver indivisibly is one *record*
/// — one tracing event, one formatted diagnostic — not one fragment of
/// it. `core::fmt` hands a sink one fragment per format piece, so a
/// record built by `write!` is gathered here before it is emitted, and
/// reaches the [`DebugConsole`] as a single segment. That is the whole
/// of the discipline: there is no second gate around it to fall out of
/// step, and nothing holds a critical section across a UART transmit.
pub struct RecordingConsole<State, TickFn> {
    state: State,
    tick_fn: TickFn,
    writer: Option<DebugSerialWriter>,
    record: String,
}

impl<State, TickFn> RecordingConsole<State, TickFn>
where
    State: ComponentRuntimeState,
    TickFn: FnMut() -> u64,
{
    /// Creates a console that records to runtime state and, when the
    /// machine's debug line carries the kernel log, mirrors each record
    /// to it through the port's own console.
    pub fn new(state: State, tick_fn: TickFn, writer: Option<DebugSerialWriter>) -> Self {
        Self {
            state,
            tick_fn,
            writer,
            record: String::with_capacity(RECORD_BUFFER_INITIAL_CAPACITY),
        }
    }

    /// Records one whole console record and mirrors it.
    ///
    /// The debugger's copy of the console and the bytes on the wire are
    /// therefore cut at the same boundaries.
    fn emit(&mut self, record: &str) {
        let ticks = (self.tick_fn)();
        self.state.record_console_text(ticks, record);
        if let Some(writer) = self.writer {
            writer.emit(record.as_bytes());
        }
    }
}

impl<State, TickFn> Write for RecordingConsole<State, TickFn>
where
    State: ComponentRuntimeState,
    TickFn: FnMut() -> u64,
{
    /// Takes one whole record.
    ///
    /// This is the path the kernel's tracing subscriber uses: it builds
    /// an event's line in a buffer of its own and hands it over in one
    /// call.
    fn write_str(&mut self, record: &str) -> fmt::Result {
        self.emit(record);
        Ok(())
    }

    /// Gathers a formatted record before emitting it.
    ///
    /// The default `write_fmt` would hand `write_str` one fragment per
    /// format piece — one for the level, the target, the message, each
    /// field — and each of those would be a segment of its own on the
    /// wire, with another processor's stage marker free to land between
    /// two of them. The record is what this console delivers whole, so
    /// the record is what reaches the port.
    fn write_fmt(&mut self, arguments: fmt::Arguments<'_>) -> fmt::Result {
        let mut record = mem::take(&mut self.record);
        let formatted = fmt::write(&mut record, arguments);
        if formatted.is_ok() {
            self.emit(&record);
        }
        if record.capacity() > RECORD_BUFFER_RETAINED_CAPACITY {
            record = String::with_capacity(RECORD_BUFFER_INITIAL_CAPACITY);
        } else {
            record.clear();
        }
        self.record = record;
        formatted
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::fmt::Write as _;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use helios_hal::serial::ByteSerial;
    use tracing::Dispatch;

    use super::RecordingConsole;
    use crate::ComponentRuntimeState;
    use crate::io::debug_serial::{DebugConsole, DebugSerialAccess, DebugSerialWriter};
    use crate::log::KernelConsoleSubscriber;

    /// The stage the debugger's marker producer announces. It is a real
    /// one: `run:begin` is what the inspector's readiness reader waits
    /// for before it opens the RPC session.
    const KDBG_STAGE: &str = "run:begin";
    /// The marker that stage puts on the wire, as its own line.
    const KDBG_MARKER: &str = "[KDBG run:begin]";
    /// Records each tracing producer emits in the contended phase.
    ///
    /// The sink writes one byte per `yield_now`, so a record of ~60
    /// bytes is ~60 chances for another processor to cut into it; a few
    /// dozen records per producer is already thousands of interleaving
    /// points, and staying well under `LOG_QUEUE_CAPACITY` keeps the
    /// test about the console rather than about queue back-pressure.
    const RECORDS_PER_PRODUCER: usize = 24;
    /// Records each producer formats through `write_fmt`.
    ///
    /// This path does not go through the log queue, so it is bounded by
    /// how long the test should run rather than by queue capacity.
    const FORMATTED_RECORDS_PER_PRODUCER: usize = 256;

    /// A sink shaped like the UART the backends actually write to: one
    /// byte at a time, with a yield between bytes, so a producer that
    /// does not hold the transmit role is split by whoever does.
    struct Wire {
        bytes: Mutex<Vec<u8>>,
    }

    impl Wire {
        fn write(&self, bytes: &[u8]) {
            for &byte in bytes {
                self.bytes
                    .lock()
                    .expect("the recorded wire was poisoned")
                    .push(byte);
                thread::yield_now();
            }
        }

        fn reset(&self) {
            self.bytes
                .lock()
                .expect("the recorded wire was poisoned")
                .clear();
        }

        /// The non-empty lines the wire carried.
        ///
        /// A stage marker opens with a newline so that it owns its line
        /// whatever preceded it, which leaves a blank line behind when
        /// the record before it ended in one; the blank lines are not
        /// what these tests are about.
        fn lines(&self) -> Vec<String> {
            let bytes = self.bytes.lock().expect("the recorded wire was poisoned");
            String::from_utf8(bytes.clone())
                .expect("the wire carried valid utf-8")
                .lines()
                .filter(|line| !line.is_empty())
                .map(ToString::to_string)
                .collect()
        }
    }

    /// Runtime state shaped like a backend's: it keeps the console text
    /// the recording console hands it, which is the copy the inspector
    /// reads back through `stats`.
    #[derive(Clone, Default)]
    struct RecordingRuntimeState {
        text: Arc<Mutex<String>>,
    }

    impl RecordingRuntimeState {
        fn lines(&self) -> Vec<String> {
            self.text
                .lock()
                .expect("the recorded console text was poisoned")
                .lines()
                .filter(|line| !line.is_empty())
                .map(ToString::to_string)
                .collect()
        }
    }

    impl ComponentRuntimeState for RecordingRuntimeState {
        fn uptime_nanos(&self, current_ticks: u64) -> u64 {
            current_ticks
        }

        fn wall_clock_offset_nanos(&self) -> i128 {
            0
        }

        fn record_console_text(&self, _: u64, text: &str) {
            self.text
                .lock()
                .expect("the recorded console text was poisoned")
                .push_str(text);
        }

        fn root_entropy(&self) -> &crate::RootEntropy {
            panic!("the console test state has no root entropy")
        }

        fn memory_balloon(&self) -> Option<crate::memory::BalloonHandle> {
            None
        }

        fn profiling_enabled(&self) -> bool {
            false
        }

        fn record_profile_stack_nanos(&self, _: crate::ProfileScope, _: String, _: u64) {}

        fn record_profile_stack_parts_nanos(
            &self,
            _: crate::ProfileScope,
            _: &str,
            _: &str,
            _: u64,
        ) {
        }

        fn record_perf_metric_parts(
            &self,
            _: crate::ProfileScope,
            _: &str,
            _: &str,
            _: crate::PerfSample,
        ) {
        }
    }

    /// Regression for #164: a `[KDBG …]` stage marker and a console
    /// record share one UART and must not splice into each other.
    ///
    /// Both producers take the real path a backend wires: the console
    /// mirrors through the port's [`DebugSerialWriter`], and the
    /// debugger emits its marker through the same writer. `write_fmt`
    /// is the console side that finds the gap, because `core::fmt`
    /// hands a sink one fragment per format piece and a marker that
    /// reached the port between two of them cut the record in half —
    /// which is what the inspector refused as
    /// `"emory balloo[KDBG boot]"`.
    #[test]
    fn a_stage_marker_never_splits_a_formatted_record() {
        struct Port;
        static WIRE: Wire = Wire {
            bytes: Mutex::new(Vec::new()),
        };
        static CONSOLE: DebugConsole = DebugConsole::new();

        impl ByteSerial for Port {
            fn try_read_byte(&self) -> Option<u8> {
                None
            }

            fn write_bytes(&self, bytes: &[u8]) {
                WIRE.write(bytes);
            }
        }

        impl DebugSerialAccess for Port {
            type Port = Self;

            fn port() -> Self {
                Self
            }

            fn console() -> &'static DebugConsole {
                &CONSOLE
            }
        }

        const WRITER: DebugSerialWriter = DebugSerialWriter::of::<Port>();

        let state = RecordingRuntimeState::default();
        let console = Arc::new(Mutex::new(RecordingConsole::new(
            state.clone(),
            || 0,
            Some(WRITER),
        )));

        let producers: Vec<_> = ["alpha", "beta"]
            .into_iter()
            .map(|producer| {
                let console = console.clone();
                thread::spawn(move || {
                    for index in 0..FORMATTED_RECORDS_PER_PRODUCER {
                        writeln!(
                            console.lock().expect("the console was poisoned"),
                            "INFO [{producer}] formatted record index={index}"
                        )
                        .expect("the recording console never fails");
                        thread::yield_now();
                    }
                })
            })
            .collect();
        let debugger = thread::spawn(move || {
            for _ in 0..FORMATTED_RECORDS_PER_PRODUCER {
                WRITER.emit_stage_marker(KDBG_STAGE);
                thread::yield_now();
            }
        });
        for producer in producers {
            producer.join().expect("a formatting producer panicked");
        }
        debugger.join().expect("the debugger producer panicked");
        let markers = FORMATTED_RECORDS_PER_PRODUCER;

        let expected: Vec<String> = ["alpha", "beta"]
            .into_iter()
            .flat_map(|producer| {
                (0..FORMATTED_RECORDS_PER_PRODUCER)
                    .map(move |index| format!("INFO [{producer}] formatted record index={index}"))
            })
            .collect();
        let lines = WIRE.lines();
        for line in &lines {
            assert!(
                line == KDBG_MARKER || expected.contains(line),
                "a stage marker and a formatted record spliced into each other: {line:?}"
            );
        }
        assert_eq!(
            lines.iter().filter(|line| *line == KDBG_MARKER).count(),
            markers,
            "a stage marker was lost or split"
        );
        assert_eq!(lines.len(), expected.len() + markers);
    }

    /// Regression for #102: a tracing record must reach the shared byte
    /// stream indivisibly, however many format pieces it is built from,
    /// and however many processors are writing.
    ///
    /// The producers are the real ones: the kernel's tracing subscriber
    /// over a recording console that mirrors through the port's writer,
    /// and the debugger emitting stage markers through that same
    /// writer.
    #[test]
    fn a_tracing_record_never_reaches_the_console_in_pieces() {
        struct Port;
        static WIRE: Wire = Wire {
            bytes: Mutex::new(Vec::new()),
        };
        static CONSOLE: DebugConsole = DebugConsole::new();

        impl ByteSerial for Port {
            fn try_read_byte(&self) -> Option<u8> {
                None
            }

            fn write_bytes(&self, bytes: &[u8]) {
                WIRE.write(bytes);
            }
        }

        impl DebugSerialAccess for Port {
            type Port = Self;

            fn port() -> Self {
                Self
            }

            fn console() -> &'static DebugConsole {
                &CONSOLE
            }
        }

        const WRITER: DebugSerialWriter = DebugSerialWriter::of::<Port>();

        let state = RecordingRuntimeState::default();
        let logger = Dispatch::new(Arc::new(KernelConsoleSubscriber::new(
            RecordingConsole::new(state.clone(), || 0, Some(WRITER)),
        )));

        // One uncontended record per producer, to learn the exact bytes
        // a whole record is made of rather than asserting against a
        // hand-written copy of the formatter's output.
        tracing::dispatcher::with_default(&logger, || {
            tracing::info!(producer = "alpha", "console record");
            tracing::info!(producer = "beta", "console record");
        });
        let expected = WIRE.lines();
        assert_eq!(
            expected.len(),
            2,
            "an uncontended record is exactly one line: {expected:?}"
        );
        WIRE.reset();
        state
            .text
            .lock()
            .expect("the recorded console text was poisoned")
            .clear();

        let producers: Vec<_> = ["alpha", "beta"]
            .into_iter()
            .map(|producer| {
                let logger = logger.clone();
                thread::spawn(move || {
                    tracing::dispatcher::with_default(&logger, || {
                        for _ in 0..RECORDS_PER_PRODUCER {
                            tracing::info!(producer, "console record");
                            thread::yield_now();
                        }
                    });
                })
            })
            .collect();
        let debugger = thread::spawn(move || {
            for _ in 0..RECORDS_PER_PRODUCER {
                WRITER.emit_stage_marker(KDBG_STAGE);
            }
        });
        for producer in producers {
            producer.join().expect("a tracing producer panicked");
        }
        debugger.join().expect("the debugger producer panicked");

        let lines = WIRE.lines();
        assert_eq!(lines.len(), RECORDS_PER_PRODUCER * 3);
        for line in &lines {
            assert!(
                line == KDBG_MARKER || expected.contains(line),
                "a record reached the console in pieces: {line:?}"
            );
        }
        for record in &expected {
            assert_eq!(
                lines.iter().filter(|line| *line == record).count(),
                RECORDS_PER_PRODUCER,
                "a producer lost or duplicated records: {record:?}"
            );
        }

        // The debugger's copy of the console is the same stream without
        // the marker producer, and it has to be whole for the same
        // reason: `stats` renders it back to the inspector.
        let recorded = state.lines();
        assert_eq!(recorded.len(), RECORDS_PER_PRODUCER * 2);
        for line in &recorded {
            assert!(
                expected.contains(line),
                "a recorded record was split by another processor: {line:?}"
            );
        }
    }
}
