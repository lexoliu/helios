//! Privileged control of the kernel's swap policy.
//!
//! The policy in force is part of the stats sample's `swap` record, so any
//! program can read it; changing it needs the privileged world this module
//! belongs to.

pub use crate::bindings::helios::system::stats::SwapPolicy;
use crate::bindings::helios::system::swap as raw;
pub use crate::bindings::helios::system::swap::PolicyError;

/// Replaces every setting of the swap policy at once.
///
/// Each setting is read where the kernel takes a decision, so the new
/// policy applies from the next one: pages already in a tier stay where
/// they are until something moves them. A refused policy changes nothing.
pub fn set_policy(policy: SwapPolicy) -> Result<(), PolicyError> {
    raw::set_policy(policy)
}
