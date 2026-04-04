//! Tracing syscalls.

use crate::bindings::helios::system::tracing as raw;
pub use crate::bindings::helios::system::tracing::subscribe;
pub use crate::bindings::helios::system::tracing::{Event, Field, Filter, Level, MonoNanos, Value};

/// Returns at most `limit` newest matching tracing events.
pub fn recent(filter: &Filter, limit: u32) -> Vec<Event> {
    raw::recent(filter, limit)
}
