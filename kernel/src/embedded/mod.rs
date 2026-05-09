//! Trusted precompiled components compiled into the kernel binary.
//!
//! The kernel keeps the payloads opaque — higher layers decide whether
//! a payload is the boot `init`, a driver, or another user-mode
//! component, and deserialise it through the artifact loader when
//! needed.
//!
//! `EmbeddedProgram` was previously a separate type with an identical
//! shape to `EmbeddedComponent`; it was exported but never used
//! outside the kernel and has been collapsed into the single type.

mod component;
mod init;

pub use component::EmbeddedComponent;
pub use init::{
    EmbeddedInit, embedded_boot_component, embedded_init, embedded_system_component,
    has_embedded_system_component,
};
