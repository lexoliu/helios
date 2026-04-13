use core::fmt::{self, Write};

use crate::ComponentRuntimeState;

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
            write_fn(s.as_bytes());
        }
        Ok(())
    }
}
