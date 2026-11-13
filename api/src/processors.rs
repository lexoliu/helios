//! The machine's processors: the possible and online sets, and taking
//! one offline and back.
//!
//! Reading the sets needs no right. A transition needs the
//! control-processors machine right ([`crate::programs::control_processors_grant`])
//! and answers `denied` without it. Transitions are serialised across
//! the machine, and each call returns once its own transition has
//! settled.

use crate::bindings::helios::system::processors as raw;
pub use crate::bindings::helios::system::processors::{
    OfflineError, OfflineRefusal, OnlineError, ProcessorId, ProcessorSets,
};

/// The possible and online sets, as the kernel holds them now.
pub fn sets() -> ProcessorSets {
    raw::sets()
}

/// Takes `processor` offline. Returns once it has stopped, or with the
/// reason it did not; a refusal leaves it online and unchanged.
pub async fn offline(processor: ProcessorId) -> Result<(), OfflineError> {
    raw::offline(processor).await
}

/// Brings `processor` online, starting it when it has never run.
/// Returns once it has joined the online set.
pub async fn online(processor: ProcessorId) -> Result<(), OnlineError> {
    raw::online(processor).await
}
