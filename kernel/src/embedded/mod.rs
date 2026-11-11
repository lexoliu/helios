//! The user payload the boot protocol hands the kernel.
//!
//! The kernel image and the user payload are separate artifacts: the
//! payload arrives as a boot module — a Limine module on x86-64 and
//! aarch64, an initrd on riscv64, a file on hosted — and is parsed at
//! bring-up into view types. [`BootPayload`] owns the parsed image,
//! [`EmbeddedInit`] is the bring-up view over it, and
//! [`EmbeddedComponent`] names one opaque component blob. The kernel
//! keeps the payload opaque — higher layers decide whether a blob is
//! the boot `init`, a driver, or another user-mode component, and
//! deserialise it through the artifact loader when needed.
//!
//! `EmbeddedProgram` was previously a separate type with an identical
//! shape to `EmbeddedComponent`; it was exported but never used
//! outside the kernel and has been collapsed into the single type.

mod component;
mod init;

pub use component::EmbeddedComponent;
pub use init::{BootPayload, EmbeddedInit};
