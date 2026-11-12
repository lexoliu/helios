//! The call interface between the kernel and the compiler kernel plugin.
//!
//! The plugin exports [`HELIOS_COMPILER_ALLOC`], [`HELIOS_COMPILER_FREE`],
//! [`HELIOS_COMPILER_COMPILE`] and [`HELIOS_COMPILER_OUTSTANDING_BYTES`] (plus
//! the two start-up exports). Every pointer and length crossing it is a `u32`
//! address in the plugin's linear memory, and both wire headers are structs of
//! `u32` fields in little-endian order, so every bit pattern the plugin or the
//! kernel writes is a valid header. Enumerated fields travel as plain integers
//! and are decoded fallibly with [`TryFrom<u32>`]; an unknown value is an
//! [`UnknownDiscriminant`], never an invalid Rust enum.
//!
//! # Ownership
//!
//! Every buffer crossing the interface has exactly one owner at a time, and
//! its owner returns it through [`HELIOS_COMPILER_FREE`] with the layout it
//! was allocated with.
//!
//! - The caller owns the four request buffers it allocates through
//!   [`HELIOS_COMPILER_ALLOC`]: the wasm input, the target string, the ISA
//!   level name and the [`CompilerRequestHeader`]. It frees each after [`HELIOS_COMPILER_COMPILE`]
//!   returns, on every path that leaves the plugin instance alive.
//! - The plugin allocates the three response buffers: the
//!   [`CompilerResponseHeader`], the precompiled output and the diagnostic.
//!   Ownership of all three passes to the caller when `compile` returns, and
//!   the caller frees each once it has copied it out.
//! - If a call into the plugin traps, or the runtime is discarded for any other
//!   reason, nothing is freed: the plugin's memory dies with its runtime.
//!
//! # Layouts
//!
//! `free` receives the `len` and `align` the buffer was allocated with, so the
//! layouts are part of the contract:
//!
//! | Buffer | `len` | `align` |
//! | --- | --- | --- |
//! | request header | [`CompilerRequestHeader::SIZE`] | [`CompilerRequestHeader::ALIGN`] |
//! | wasm input, target string, ISA level name | their byte length | 1 |
//! | response header | [`CompilerResponseHeader::SIZE`] | [`CompilerResponseHeader::ALIGN`] |
//! | precompiled output, diagnostic | the `len` field of the response header | 1 |
//!
//! A zero-length buffer has pointer 0 and is never allocated or freed, in
//! either direction. The plugin hands the output and the diagnostic over as
//! boxed slices, so their allocation is exactly `len` bytes.
//!
//! # Accounting
//!
//! The plugin counts the bytes currently owned across the interface:
//! [`HELIOS_COMPILER_ALLOC`] and each response allocation it hands out add
//! their length, [`HELIOS_COMPILER_FREE`] subtracts it. The count is
//! [`HELIOS_COMPILER_OUTSTANDING_BYTES`]; a caller that frees what it owns
//! sees it return to its starting level after every compile, and the kernel
//! checks that it does: a count left off that level fails the call and
//! discards the plugin's runtime.

#![no_std]

use thiserror::Error;

/// Version 2 carries `hint`, `status` and `output_kind` as plain `u32` and
/// states the ownership and layout contract above. Version 3 adds the target's
/// ISA level name to the request header as a fourth caller-owned request
/// buffer. Version 4 retires hint value 0: the plugin compiles with Cranelift
/// only, so `Balanced` and `Performance` are the only hints, and 0 is an
/// [`UnknownDiscriminant`] like any other.
pub const HELIOS_COMPILER_ABI_VERSION: u32 = 4;

pub const HELIOS_COMPILER_ALLOC: &str = "helios_compiler_alloc";
pub const HELIOS_COMPILER_FREE: &str = "helios_compiler_free";
pub const HELIOS_COMPILER_COMPILE: &str = "helios_compiler_compile";
pub const HELIOS_COMPILER_OUTSTANDING_BYTES: &str = "helios_compiler_outstanding_bytes";
pub const HELIOS_COMPILER_INITIALIZE: &str = "helios_compiler_initialize";
pub const HELIOS_COMPILER_PTHREAD_SELF_OFFSET: &str = "helios_compiler_pthread_self_offset";
pub const HELIOS_COMPILER_REQUEST_PROFILE: u32 = 1 << 0;

/// A wire integer that names no variant of the enum it was decoded into.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("{field} {value} is not a known value")]
pub struct UnknownDiscriminant {
    /// The header field that carried the value.
    pub field: &'static str,
    pub value: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileHint {
    Balanced,
    Performance,
}

impl From<CompileHint> for u32 {
    fn from(hint: CompileHint) -> Self {
        match hint {
            CompileHint::Balanced => 1,
            CompileHint::Performance => 2,
        }
    }
}

impl TryFrom<u32> for CompileHint {
    type Error = UnknownDiscriminant;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Balanced),
            2 => Ok(Self::Performance),
            value => Err(UnknownDiscriminant {
                field: "hint",
                value,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompilerStatus {
    Ok,
    InvalidRequest,
    InvalidInput,
    CompileFailed,
}

impl From<CompilerStatus> for u32 {
    fn from(status: CompilerStatus) -> Self {
        match status {
            CompilerStatus::Ok => 0,
            CompilerStatus::InvalidRequest => 1,
            CompilerStatus::InvalidInput => 2,
            CompilerStatus::CompileFailed => 3,
        }
    }
}

impl TryFrom<u32> for CompilerStatus {
    type Error = UnknownDiscriminant;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::InvalidRequest),
            2 => Ok(Self::InvalidInput),
            3 => Ok(Self::CompileFailed),
            value => Err(UnknownDiscriminant {
                field: "status",
                value,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    CoreModule,
    Component,
}

impl From<OutputKind> for u32 {
    fn from(kind: OutputKind) -> Self {
        match kind {
            OutputKind::CoreModule => 0,
            OutputKind::Component => 1,
        }
    }
}

impl TryFrom<u32> for OutputKind {
    type Error = UnknownDiscriminant;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::CoreModule),
            1 => Ok(Self::Component),
            value => Err(UnknownDiscriminant {
                field: "output_kind",
                value,
            }),
        }
    }
}

/// Defines a wire header of `u32` fields together with its byte encoding.
///
/// The struct is `repr(C)` so its in-memory layout equals the wire layout, and
/// the encoding is explicit little-endian so neither side reinterprets
/// memory: every `[u8; SIZE]` decodes to a valid header.
macro_rules! wire_header {
    ($(#[$meta:meta])* $name:ident { $($(#[$field_meta:meta])* $field:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name {
            $($(#[$field_meta])* pub $field: u32,)+
        }

        impl $name {
            /// Encoded length in bytes, and the `len` the header is
            /// allocated with.
            pub const SIZE: usize = core::mem::size_of::<Self>();
            /// The `align` the header is allocated with.
            pub const ALIGN: usize = core::mem::align_of::<Self>();

            /// The header's wire bytes.
            pub fn to_bytes(&self) -> [u8; Self::SIZE] {
                let mut bytes = [0_u8; Self::SIZE];
                let mut fields = bytes.chunks_exact_mut(4);
                $(
                    fields
                        .next()
                        .expect("header has a chunk per field")
                        .copy_from_slice(&self.$field.to_le_bytes());
                )+
                bytes
            }

            /// Decodes a header; every byte pattern is a valid header.
            pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
                let mut fields = bytes.chunks_exact(4);
                Self {
                    $($field: u32::from_le_bytes(
                        fields
                            .next()
                            .expect("header has a chunk per field")
                            .try_into()
                            .expect("chunk is four bytes"),
                    ),)+
                }
            }
        }
    };
}

wire_header! {
    /// What the caller writes into the request header buffer.
    CompilerRequestHeader {
        abi_version,
        /// A [`CompileHint`].
        hint,
        wasm_ptr,
        wasm_len,
        target_ptr,
        target_len,
        /// The name of the target's ISA level
        /// (`helios_artifact::IsaLevel::name`) the artifact is compiled for.
        isa_level_ptr,
        isa_level_len,
        /// `HELIOS_COMPILER_REQUEST_*` bits.
        flags,
    }
}

wire_header! {
    /// What the plugin writes into the response header buffer it returns.
    CompilerResponseHeader {
        abi_version,
        /// A [`CompilerStatus`].
        status,
        /// An [`OutputKind`].
        output_kind,
        precompiled_ptr,
        precompiled_len,
        diagnostic_ptr,
        diagnostic_len,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn hint_round_trips() {
        for hint in [CompileHint::Balanced, CompileHint::Performance] {
            assert_eq!(CompileHint::try_from(u32::from(hint)), Ok(hint));
        }
    }

    /// Value 0 was the `Fast` hint; it decodes to the error any unknown value
    /// does.
    #[test]
    fn the_retired_fast_hint_value_is_refused() {
        assert_eq!(
            CompileHint::try_from(0),
            Err(UnknownDiscriminant {
                field: "hint",
                value: 0
            })
        );
    }

    #[test]
    fn status_round_trips() {
        for status in [
            CompilerStatus::Ok,
            CompilerStatus::InvalidRequest,
            CompilerStatus::InvalidInput,
            CompilerStatus::CompileFailed,
        ] {
            assert_eq!(CompilerStatus::try_from(u32::from(status)), Ok(status));
        }
    }

    #[test]
    fn output_kind_round_trips() {
        for kind in [OutputKind::CoreModule, OutputKind::Component] {
            assert_eq!(OutputKind::try_from(u32::from(kind)), Ok(kind));
        }
    }

    #[test]
    fn out_of_range_values_name_their_field() {
        assert_eq!(
            CompileHint::try_from(3),
            Err(UnknownDiscriminant {
                field: "hint",
                value: 3
            })
        );
        assert_eq!(
            CompilerStatus::try_from(4),
            Err(UnknownDiscriminant {
                field: "status",
                value: 4
            })
        );
        assert_eq!(
            OutputKind::try_from(u32::MAX),
            Err(UnknownDiscriminant {
                field: "output_kind",
                value: u32::MAX
            })
        );
    }

    #[test]
    fn header_sizes_are_word_multiples() {
        assert_eq!(CompilerRequestHeader::SIZE, 36);
        assert_eq!(CompilerResponseHeader::SIZE, 28);
        assert_eq!(CompilerRequestHeader::ALIGN, 4);
        assert_eq!(CompilerResponseHeader::ALIGN, 4);
    }

    #[test]
    fn request_header_round_trips_in_field_order() {
        let header = CompilerRequestHeader {
            abi_version: HELIOS_COMPILER_ABI_VERSION,
            hint: CompileHint::Balanced.into(),
            wasm_ptr: 0x1000,
            wasm_len: 12,
            target_ptr: 0x2000,
            target_len: 8,
            isa_level_ptr: 0x3000,
            isa_level_len: 6,
            flags: HELIOS_COMPILER_REQUEST_PROFILE,
        };
        let bytes = header.to_bytes();
        assert_eq!(&bytes[..4], &3_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &1_u32.to_le_bytes());
        assert_eq!(&bytes[24..28], &0x3000_u32.to_le_bytes());
        assert_eq!(
            &bytes[32..36],
            &HELIOS_COMPILER_REQUEST_PROFILE.to_le_bytes()
        );
        assert_eq!(CompilerRequestHeader::from_bytes(&bytes), header);
    }

    #[test]
    fn response_header_round_trips_and_keeps_unknown_values() {
        let header = CompilerResponseHeader {
            abi_version: HELIOS_COMPILER_ABI_VERSION,
            status: 99,
            output_kind: 77,
            precompiled_ptr: 1,
            precompiled_len: 2,
            diagnostic_ptr: 3,
            diagnostic_len: 4,
        };
        let decoded = CompilerResponseHeader::from_bytes(&header.to_bytes());
        assert_eq!(decoded, header);
        assert!(CompilerStatus::try_from(decoded.status).is_err());
        assert!(OutputKind::try_from(decoded.output_kind).is_err());
    }
}
