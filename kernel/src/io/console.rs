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
pub fn emit_console_line(emit: impl FnOnce()) {
    critical_section::with(|_| emit());
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
}

impl<State, TickFn, WriteFn> Write for RecordingConsole<State, TickFn, WriteFn>
where
    State: ComponentRuntimeState,
    TickFn: FnMut() -> u64,
    WriteFn: FnMut(&[u8]),
{
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let ticks = (self.tick_fn)();
        self.state.record_console_text(ticks, s);
        if let Some(write_fn) = &mut self.write_fn {
            emit_console_line(|| write_fn(s.as_bytes()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use helios_hal::serial::ByteSerial;

    use super::emit_console_line;

    const KDBG_MARKER: &str = "[KDBG run:begin]";
    const TRACE_LINE: &str = "INFO [helios_kernel::memory::balloon] reporting started";
    const LINES_PER_PRODUCER: usize = 200;

    /// A sink shaped like the UART the backends actually write to: one byte at
    /// a time, with a yield between bytes so an ungated producer is split.
    #[derive(Default)]
    struct RecordingSink {
        bytes: Mutex<Vec<u8>>,
    }

    impl RecordingSink {
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
}
