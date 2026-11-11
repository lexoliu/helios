//! The Venus wire protocol: Vulkan commands serialized for a renderer that
//! runs them on a host GPU.
//!
//! [`runtime`] is the hand-written half: the encoding and decoding sinks and
//! the primitives. [`protocol`] is generated at build time from the pinned
//! venus-protocol registry by `helios-venus-gen`: Rust-owned `#[repr(C)]`
//! types for the Vulkan structures, the command type constants, and for every
//! serializable command its size, its encoding, the size of its reply and the
//! decoding of its reply.
//!
//! The generated functions take raw pointers to Vulkan-layout memory, as the
//! C functions they mirror do, and are `unsafe` on the same terms: the
//! pointers, array lengths and `pNext` chains describe live memory.

pub mod runtime;

pub use runtime::*;

/// The generated protocol. Its names are those of `vk.xml` and Mesa's
/// generated headers, so the two can be compared by eye and by test.
#[expect(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    clippy::missing_safety_doc,
    clippy::too_many_arguments,
    clippy::manual_div_ceil,
    clippy::unnecessary_cast,
    unused_variables,
    unused_unsafe,
    reason = "generated from vk.xml: Vulkan names, raw-pointer entry points that mirror the C ones, commands with as many parameters as the registry gives them, length expressions copied from the registry as written (`(samples + 31) / 32`, `size as u64` of a 64-bit size), reply walks that take every parameter of the command but read only its outputs, and structs whose members are all outputs"
)]
pub mod protocol {
    use crate::runtime::scalar_newtype;
    use crate::runtime::*;

    include!(concat!(env!("OUT_DIR"), "/protocol.rs"));
}
