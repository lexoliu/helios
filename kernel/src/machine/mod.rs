//! The machine as a whole, as distinct from any one device or subsystem.

mod generation;
mod reactions;

#[cfg(test)]
pub(crate) use generation::test_window;
pub use generation::{
    AdvanceCause, GENERATION_PROBE_INTERVAL, Generation, MachineGeneration,
    MachineGenerationHandle, UnhandledAdvances, spawn_generation_probe,
};
pub use reactions::install_generation_reactions;
