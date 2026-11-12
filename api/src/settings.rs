//! Machine-wide settings: the operator's values for limits the kernel
//! enforces.
//!
//! A setting is the value new processes and new requests take; changing one
//! never reaches into a process that is already running. Names are
//! `subsystem.name`, and the kernel's list is the only list: [`list`] says
//! what exists, so a caller never hard-codes a name it did not get from the
//! kernel.

use crate::bindings::helios::system::settings as raw;
pub use crate::bindings::helios::system::settings::{Setting, Unit, Value, WriteError};

/// Returns every setting, in the kernel's order. Needs no right.
pub fn list() -> Vec<Setting> {
    raw::list()
}

/// Returns the setting named `name`, or `None` when the kernel has none by
/// that name. Needs no right.
pub fn read(name: &str) -> Option<Setting> {
    raw::read(name)
}

/// Makes `value` the setting's value for every process and request created
/// from now on, and returns the value it replaced.
///
/// Refused with [`WriteError::Denied`] unless the instance holds the right
/// to write settings, before the name is looked up; with
/// [`WriteError::Unknown`] when no setting has that name; and with
/// [`WriteError::UnitMismatch`], carrying the unit the setting holds, when
/// `value` is in another unit.
pub fn write(name: &str, value: Value) -> Result<Value, WriteError> {
    raw::write(name, value)
}
