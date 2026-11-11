//! The machine as a whole, as distinct from any one device or subsystem.

mod generation;

#[cfg(test)]
pub(crate) use generation::test_window;
pub use generation::{AdvanceCause, Generation, MachineGeneration, MachineGenerationHandle};
