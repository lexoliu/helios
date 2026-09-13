//! Tracing syscalls.

use crate::bindings::helios::system::tracing as raw;
pub use crate::bindings::helios::system::tracing::subscribe;
pub use crate::bindings::helios::system::tracing::{
    Event, Field, Filter, Level, MonoNanos, TargetError, Value,
};

/// Enables or disables a kernel diagnostic target by name for the rest
/// of the boot.
pub fn set_target_enabled(target: &str, enabled: bool) -> Result<(), TargetError> {
    raw::set_target_enabled(target, enabled)
}

/// Returns at most `limit` newest matching tracing events.
pub fn recent(filter: &Filter, limit: u32) -> Vec<Event> {
    raw::recent(filter, limit)
}
