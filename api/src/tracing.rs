//! Tracing syscalls.

pub use crate::bindings::helios::system::filters::FilterError;
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
///
/// A filter of more than 8 prefixes, or with a prefix longer than 128
/// bytes, is refused with the limit it exceeded.
pub fn recent(filter: &Filter, limit: u32) -> Result<Vec<Event>, FilterError> {
    raw::recent(filter, limit)
}
