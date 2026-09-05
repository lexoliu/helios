use core::fmt::{self, Write};

use crate::ComponentRuntimeState;

/// Emits one console message with the console gate held, so the bytes reach
/// the shared byte stream indivisibly.
///
/// Every kernel console producer lands on the same device: the tracing
/// mirror, the embedded debugger's `[KDBG …]` stage markers, and panic
/// output all reach one UART on every backend. None of them writes in a
/// single store — a UART sink pushes byte by byte, and `write_fmt` hands the
/// sink one fragment per format piece — so two producers running on two
/// processors split each other's lines. That is how a stage marker reached
/// the inspector as `"[KDBG eng152 "`, with a balloon trace line wedged
/// through the middle of it.
///
/// The gate is the critical section, which is what makes this work across
/// processors as well as across the interrupts of one. `emit` must write a
/// complete message and must not await: the section is held for exactly as
/// long as the message takes to reach the device.
///
/// What reaches the device is a separate question, and its answer is
/// [`DebugConsole`](super::DebugConsole): the port has one owner, and a
/// record handed to it is written whole whoever else is writing. This
/// gate is what establishes the *record* — where one begins and ends
/// across the fragments `core::fmt` hands a sink, and that the
/// debugger's copy of the console is cut at the same boundaries as the
/// bytes on the wire.
///
/// The unit of a message is one *record* — one tracing event, one stage
/// marker, one panic report — not one fragment of it. A record carries its
/// own bound: it is as long as the formatter makes it and no longer, so
/// holding the gate across a whole record holds it for a bounded time
/// while holding it per fragment holds it for no useful invariant at all.
/// The section nests, so a producer that already owns the gate may call
/// through another gated writer without deadlocking.
pub fn emit_console_line<Emitted>(emit: impl FnOnce() -> Emitted) -> Emitted {
    critical_section::with(|_| emit())
}

/// Generic recording console that traces output into kernel runtime state
/// and optionally mirrors bytes to a hardware serial sink.
///
/// Backends instantiate this with their architecture-specific tick source
/// and byte writer. This eliminates the duplicated console wrapper pattern
/// across riscv and x86.
pub struct RecordingConsole<State, TickFn, WriteFn> {
    state: State,
    tick_fn: TickFn,
    write_fn: Option<WriteFn>,
}

impl<State, TickFn, WriteFn> RecordingConsole<State, TickFn, WriteFn>
where
    State: ComponentRuntimeState,
    TickFn: FnMut() -> u64,
    WriteFn: FnMut(&[u8]),
{
    /// Create a console that records to runtime state and optionally mirrors
    /// to a byte sink.
    pub fn new(state: State, tick_fn: TickFn, write_fn: Option<WriteFn>) -> Self {
        Self {
            state,
            tick_fn,
            write_fn,
        }
    }

    /// Records one piece of console text and mirrors it to the sink.
    ///
    /// Callers hold the console gate around a whole record; this is the
    /// body that runs inside it, so the debugger's copy of the console
    /// and the bytes on the wire are cut at the same boundaries.
    fn emit(&mut self, text: &str) {
        let ticks = (self.tick_fn)();
        self.state.record_console_text(ticks, text);
        if let Some(write_fn) = &mut self.write_fn {
            write_fn(text.as_bytes());
        }
    }
}

impl<State, TickFn, WriteFn> Write for RecordingConsole<State, TickFn, WriteFn>
where
    State: ComponentRuntimeState,
    TickFn: FnMut() -> u64,
    WriteFn: FnMut(&[u8]),
{
    fn write_str(&mut self, s: &str) -> fmt::Result {
        emit_console_line(|| self.emit(s));
        Ok(())
    }

    /// Holds the gate across the whole formatted record.
    ///
    /// `core::fmt` hands a sink one fragment per format piece, so the
    /// default `write_fmt` would take and release the gate several times
    /// per record — once per level, target, message and field — and
    /// another processor's stage marker would slot into any of those
    /// gaps. The record is the message this console promises to deliver
    /// indivisibly, so the record is what the gate spans; `write_str`
    /// nests inside it.
    fn write_fmt(&mut self, arguments: fmt::Arguments<'_>) -> fmt::Result {
        emit_console_line(|| fmt::write(self, arguments))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::fmt::Write as _;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use helios_hal::serial::ByteSerial;
    use tracing::Dispatch;

    use super::{RecordingConsole, emit_console_line};
    use crate::ComponentRuntimeState;
    use crate::log::KernelConsoleSubscriber;

    const KDBG_MARKER: &str = "[KDBG run:begin]";
    const TRACE_LINE: &str = "INFO [helios_kernel::memory::balloon] reporting started";
    const LINES_PER_PRODUCER: usize = 200;
    /// Records each tracing producer emits in the contended phase.
    ///
    /// The sink writes one byte per `yield_now`, so a record of ~60 bytes
    /// is ~60 chances for another processor to cut into it; a few dozen
    /// records per producer is already thousands of interleaving points,
    /// and staying well under `LOG_QUEUE_CAPACITY` keeps the test about
    /// the console gate rather than about queue back-pressure.
    const RECORDS_PER_PRODUCER: usize = 24;
    /// Records each producer formats through `write_fmt`.
    ///
    /// This path does not go through the log queue, so it is bounded by
    /// how long the test should run rather than by queue capacity.
    const FORMATTED_RECORDS_PER_PRODUCER: usize = 256;

    /// A sink shaped like the UART the backends actually write to: one byte at
    /// a time, with a yield between bytes so an ungated producer is split.
    #[derive(Default)]
    struct RecordingSink {
        bytes: Mutex<Vec<u8>>,
    }

    impl RecordingSink {
        fn reset(&self) {
            self.bytes
                .lock()
                .expect("the recording sink was poisoned")
                .clear();
        }

        fn lines(&self) -> Vec<String> {
            let bytes = self.bytes.lock().expect("the recording sink was poisoned");
            String::from_utf8(bytes.clone())
                .expect("the sink recorded valid utf-8")
                .lines()
                .map(ToString::to_string)
                .collect()
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

    fn produce(sink: &RecordingSink, line: &str) {
        for _ in 0..LINES_PER_PRODUCER {
            emit_console_line(|| {
                sink.write_bytes(line.as_bytes());
                sink.write_bytes(b"\n");
            });
        }
    }

    /// Regression for #76: a stage marker reached the inspector as
    /// `"[KDBG eng152 "` because the tracing mirror and the debugger wrote to
    /// one UART without a shared gate, and each of them writes byte by byte.
    #[test]
    fn two_producers_never_split_each_others_lines() {
        let sink = Arc::new(RecordingSink::default());
        let debugger = {
            let sink = sink.clone();
            thread::spawn(move || produce(&sink, KDBG_MARKER))
        };
        let tracing = {
            let sink = sink.clone();
            thread::spawn(move || produce(&sink, TRACE_LINE))
        };
        debugger.join().expect("the debugger producer panicked");
        tracing.join().expect("the tracing producer panicked");

        let lines = sink.lines();
        assert_eq!(lines.len(), LINES_PER_PRODUCER * 2);
        for line in &lines {
            assert!(
                line == KDBG_MARKER || line == TRACE_LINE,
                "a producer's line was split by the other: {line:?}"
            );
        }
        assert_eq!(
            lines.iter().filter(|line| *line == KDBG_MARKER).count(),
            LINES_PER_PRODUCER
        );
    }

    /// Runtime state shaped like a backend's: it keeps the console text
    /// the recording console hands it, which is the copy the inspector
    /// reads back through `stats`.
    #[derive(Clone, Default)]
    struct RecordingRuntimeState {
        text: Arc<Mutex<String>>,
    }

    impl RecordingRuntimeState {
        fn reset(&self) {
            self.text
                .lock()
                .expect("the recorded console text was poisoned")
                .clear();
        }

        fn lines(&self) -> Vec<String> {
            self.text
                .lock()
                .expect("the recorded console text was poisoned")
                .lines()
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
            panic!("the console gate test state has no root entropy")
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

    /// The kernel logger, wired to a recording console over the
    /// byte-at-a-time sink, exactly as a backend wires it.
    fn kernel_logger(state: RecordingRuntimeState, sink: Arc<RecordingSink>) -> Dispatch {
        let console = RecordingConsole::new(
            state,
            || 0,
            Some(move |bytes: &[u8]| sink.write_bytes(bytes)),
        );
        Dispatch::new(Arc::new(KernelConsoleSubscriber::new(console)))
    }

    /// A record built by `write!` is one message too.
    ///
    /// `core::fmt` splits a format string into one fragment per piece —
    /// here five of them — so this is the path where a per-fragment gate
    /// shows up as a marker cut in half. The debugger keeps emitting
    /// stage markers for as long as the producers keep formatting, which
    /// is the traffic that finds the gaps.
    #[test]
    fn a_formatted_record_never_reaches_the_console_in_pieces() {
        let sink = Arc::new(RecordingSink::default());
        let state = RecordingRuntimeState::default();
        let console = Arc::new(Mutex::new(RecordingConsole::new(
            state.clone(),
            || 0,
            Some({
                let sink = sink.clone();
                move |bytes: &[u8]| sink.write_bytes(bytes)
            }),
        )));
        let formatting = Arc::new(AtomicBool::new(true));

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
        let debugger = {
            let sink = sink.clone();
            let formatting = formatting.clone();
            thread::spawn(move || {
                let mut markers = 0_usize;
                while formatting.load(Ordering::Acquire) {
                    emit_console_line(|| {
                        sink.write_bytes(KDBG_MARKER.as_bytes());
                        sink.write_bytes(b"\n");
                    });
                    markers += 1;
                }
                markers
            })
        };
        for producer in producers {
            producer.join().expect("a formatting producer panicked");
        }
        formatting.store(false, Ordering::Release);
        let markers = debugger.join().expect("the debugger producer panicked");

        let expected: Vec<String> = ["alpha", "beta"]
            .into_iter()
            .flat_map(|producer| {
                (0..FORMATTED_RECORDS_PER_PRODUCER)
                    .map(move |index| format!("INFO [{producer}] formatted record index={index}"))
            })
            .collect();
        let lines = sink.lines();
        assert_eq!(lines.len(), expected.len() + markers);
        for line in &lines {
            assert!(
                line == KDBG_MARKER || expected.contains(line),
                "a formatted record reached the console in pieces: {line:?}"
            );
        }
        assert_eq!(
            lines.iter().filter(|line| *line == KDBG_MARKER).count(),
            markers
        );
    }

    /// Regression for #102: a tracing record must reach the shared byte
    /// stream indivisibly, however many format pieces it is built from.
    ///
    /// `Write::write_str` is a fragment-granularity interface — nothing
    /// in `core::fmt` promises a caller hands a sink one whole message —
    /// so the record boundary has to be established before the console,
    /// and the console gate then covers the record rather than a piece of
    /// it. Two processors emitting records while a third emits `[KDBG …]`
    /// stage markers is the traffic that produced a marker cut in half on
    /// the x86 bench lane.
    #[test]
    fn a_tracing_record_never_reaches_the_console_in_pieces() {
        let sink = Arc::new(RecordingSink::default());
        let state = RecordingRuntimeState::default();
        let logger = kernel_logger(state.clone(), sink.clone());

        // One uncontended record per producer, to learn the exact bytes a
        // whole record is made of rather than asserting against a
        // hand-written copy of the formatter's output.
        tracing::dispatcher::with_default(&logger, || {
            tracing::info!(producer = "alpha", "console gate record");
            tracing::info!(producer = "beta", "console gate record");
        });
        let expected = sink.lines();
        assert_eq!(
            expected.len(),
            2,
            "an uncontended record is exactly one line: {expected:?}"
        );
        sink.reset();
        state.reset();

        let producers: Vec<_> = ["alpha", "beta"]
            .into_iter()
            .map(|producer| {
                let logger = logger.clone();
                thread::spawn(move || {
                    tracing::dispatcher::with_default(&logger, || {
                        for _ in 0..RECORDS_PER_PRODUCER {
                            tracing::info!(producer, "console gate record");
                            thread::yield_now();
                        }
                    });
                })
            })
            .collect();
        let debugger = {
            let sink = sink.clone();
            thread::spawn(move || {
                for _ in 0..RECORDS_PER_PRODUCER {
                    emit_console_line(|| {
                        sink.write_bytes(KDBG_MARKER.as_bytes());
                        sink.write_bytes(b"\n");
                    });
                }
            })
        };
        for producer in producers {
            producer.join().expect("a tracing producer panicked");
        }
        debugger.join().expect("the debugger producer panicked");

        let lines = sink.lines();
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
