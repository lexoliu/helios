//! The machine as a whole, as distinct from any one device or subsystem.

mod generation;
mod reactions;

#[cfg(test)]
pub(crate) use generation::test_window;
pub use generation::{
    AdvanceCause, Generation, MachineGeneration, MachineGenerationHandle, UnhandledAdvances,
};
pub use reactions::install_generation_reactions;
