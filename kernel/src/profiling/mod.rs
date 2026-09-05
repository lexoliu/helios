//! The kernel's own LLVM profile runtime.
//!
//! A kernel built with `-C profile-generate -Z no-profiler-runtime` carries
//! LLVM's instrumentation — counters in `__llvm_prf_cnts`, per-function
//! records in `__llvm_prf_data`, symbol names in `__llvm_prf_names` — but
//! none of compiler-rt's `InstrProfiling*.c`, which assume a libc. This
//! module is what replaces them: the `__llvm_profile_runtime` symbol every
//! instrumented object references, and a `.profraw` writer over the linked
//! sections.
//!
//! Nothing here allocates and nothing here is reachable in a kernel that was
//! not instrumented: the writer, the sections and the runtime symbol are
//! compiled only under `--cfg helios_profile_generate`, which the
//! `profile-generate` build sets in the same rustflags that turn the
//! instrumentation on (`docs/pgo.md`). A plain kernel still answers the
//! `helios:system/profiling` export — with [`LlvmProfileError::NotInstrumented`],
//! never with an empty profile.

#[cfg(any(helios_profile_generate, test))]
mod raw;

#[cfg(helios_profile_generate)]
mod instrumented;
#[cfg(not(helios_profile_generate))]
mod plain;

/// Largest window [`LlvmProfile::read`] serves in one call.
///
/// The raw profile of an instrumented kernel is tens of megabytes, so it
/// leaves the kernel a window at a time: the caller asks for the size, then
/// walks the image. The cap bounds the one transient buffer the component
/// host lowers into the guest.
pub const MAX_PROFILE_READ: u32 = 256 * 1024;

/// One of the sections the kernel's instrumentation is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSection {
    /// `__llvm_prf_cnts`, one counter per instrumented region.
    Counters,
    /// `__llvm_prf_data`, one record per instrumented function.
    Data,
    /// `__llvm_prf_names`, the compressed function names.
    Names,
}

impl core::fmt::Display for ProfileSection {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::Counters => "__llvm_prf_cnts",
            Self::Data => "__llvm_prf_data",
            Self::Names => "__llvm_prf_names",
        };
        formatter.write_str(name)
    }
}

/// Why the kernel could not produce an LLVM raw profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LlvmProfileError {
    /// This kernel was not built with `-C profile-generate`, so there is no
    /// instrumentation to export.
    #[error(
        "kernel image carries no LLVM instrumentation: build it with the profile-generate profile"
    )]
    NotInstrumented,
    /// The instrumentation in the image is a raw-profile version, or a
    /// variant of one, that this writer does not implement.
    #[error(
        "kernel instrumentation reports raw profile version word {found:#018x}, and this writer implements version {implemented}"
    )]
    UnsupportedVersion {
        /// `__llvm_profile_raw_version` as the instrumented image carries it.
        found: u64,
        /// The version [`raw`] serialises.
        implemented: u64,
    },
    /// A section the raw profile is made of is not a whole number of its
    /// records, so the image the linker produced is not the one the format
    /// describes.
    #[error(
        "{section} section is {len} bytes, which is not a multiple of its {record_len}-byte record"
    )]
    MalformedSection {
        /// The `__llvm_prf_*` section the linker sized wrongly.
        section: ProfileSection,
        /// Byte length the linker gave it.
        len: u64,
        /// Record size the format fixes.
        record_len: u64,
    },
    /// The caller asked for a window that starts past the end of the profile.
    #[error("offset {offset} is past the end of the {len}-byte raw profile")]
    OutOfRange {
        /// Offset the caller asked for.
        offset: u64,
        /// Length of the profile.
        len: u64,
    },
    /// The caller asked for a window larger than [`MAX_PROFILE_READ`].
    #[error("requested {requested} bytes, and one read serves at most {limit}")]
    ReadTooLarge {
        /// Window the caller asked for.
        requested: u64,
        /// [`MAX_PROFILE_READ`].
        limit: u32,
    },
}

/// The kernel's LLVM raw-profile export.
///
/// The system capability is this trait; `helios:system/profiling` is its WIT
/// spelling. Both implementations are compile-time selected by the build
/// profile, so nothing probes for instrumentation at runtime.
pub trait LlvmProfile {
    /// Total length of the `.profraw` image, in bytes.
    ///
    /// The length is fixed by the link, not by what has executed, so a caller
    /// may ask once and then walk the image while the kernel keeps counting.
    fn size(&self) -> Result<u64, LlvmProfileError>;

    /// Copies the window of the image starting at `offset` into `out`,
    /// returning how many bytes it wrote. A window that reaches the end of
    /// the image is short; `offset == size()` writes nothing.
    fn read(&self, offset: u64, out: &mut [u8]) -> Result<usize, LlvmProfileError>;
}

#[cfg(helios_profile_generate)]
pub use instrumented::InstrumentedKernelProfile as KernelLlvmProfile;
#[cfg(not(helios_profile_generate))]
pub use plain::PlainKernelProfile as KernelLlvmProfile;
