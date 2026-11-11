//! The machine as a whole, as distinct from any one device or subsystem.

mod generation;

#[cfg(test)]
pub(crate) use generation::test_window;
pub use generation::{
    AdvanceCause, GENERATION_PROBE_INTERVAL, Generation, MachineGeneration,
    MachineGenerationHandle, spawn_generation_probe,
};
