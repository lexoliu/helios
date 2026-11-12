//! Build-time generator that turns the pinned venus-protocol registry into the
//! Rust wire protocol `helios-venus` includes.
//!
//! [`inputs`] is always available: it names, locates and verifies the pinned
//! files. Everything else is behind the `generate` feature.

pub mod inputs;

#[cfg(feature = "generate")]
pub mod analysis;
#[cfg(feature = "generate")]
pub mod emit;
#[cfg(feature = "generate")]
pub mod expr;
#[cfg(feature = "generate")]
pub mod layout;
#[cfg(feature = "generate")]
pub mod policy;
#[cfg(feature = "generate")]
pub mod registry;
