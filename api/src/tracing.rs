//! Tracing syscalls.

pub use crate::bindings::helios::system::filters::FilterError;
use crate::bindings::helios::system::tracing as raw;
pub use crate::bindings::helios::system::tracing::subscribe;
pub use crate::bindings::helios::system::tracing::{
    Event, Field, Filter, Level, MonoNanos, TargetError, Value,
};

/// Enables or disables a kernel diagnostic target by name for the rest
/// of the boot.
///
/// Refused with [`TargetError::Denied`] unless the instance holds the
/// right to observe the machine.
pub fn set_target_enabled(target: &str, enabled: bool) -> Result<(), TargetError> {
    raw::set_target_enabled(target, enabled)
}

/// Returns at most `limit` newest matching tracing events.
///
/// An instance that holds the right to observe the machine sees every
/// event; any other sees only the events it wrote.
///
/// A filter of more than 8 prefixes, or with a prefix longer than 128
/// bytes, is refused with the limit it exceeded.
pub fn recent(filter: &Filter, limit: u32) -> Result<Vec<Event>, FilterError> {
    raw::recent(filter, limit)
}
