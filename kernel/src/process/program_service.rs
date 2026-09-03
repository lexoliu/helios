use core::fmt::{self, Write};

use arrayvec::ArrayString;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProgramExecErrorKind {
    #[error("the program artifact is invalid")]
    InvalidBinary,
    #[error("the program artifact exports no supported entry point")]
    MissingEntry,
    #[error("the program artifact imports unsupported host functions")]
    UnsupportedImport,
    #[error("the program artifact signature is invalid or untrusted")]
    InvalidSignature,
    #[error("the requested program path is invalid")]
    InvalidPath,
    #[error("the requested operation is not allowed by process authority")]
    PermissionDenied,
    #[error("the requested AOT hint is invalid for this input")]
    InvalidHint,
    #[error("the program exhausted its memory budget")]
    OutOfMemory,
    #[error("program exec is unavailable on this platform")]
    Unavailable,
    #[error("the kernel rejected the program for an internal reason")]
    Internal,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error(
    "program memory request of {requested_bytes} bytes exceeds its memory budget: available={available_bytes} reserved={reserved_bytes}"
)]
pub struct ProgramOutOfMemory {
    pub requested_bytes: usize,
    pub available_bytes: usize,
    pub reserved_bytes: usize,
}

/// How much of a runtime message travels with a [`ProgramExecError`].
///
/// Long enough for a wasmtime error's own sentence, short enough that every
/// program error stays a stack value in a kernel that does not allocate on
/// failure paths — and small enough that `ProgramExecError` still fits the
/// budget asserted below, since the error travels by value through every
/// `Result` on the program path.
const RUNTIME_MESSAGE_CAPACITY: usize = 112;

/// What the runtime itself said about a failure the kernel has no name for.
///
/// [`ProgramExecErrorDetail`] names every failure the kernel recognises;
/// `RuntimeFailure` is by construction the one it does not. Dropping the
/// runtime's own message there leaves a caller — the inspector, a guest
/// program, a CI log — with `runtime operation failed` and nothing to act on,
/// so the message travels with the error instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeMessage(ArrayString<RUNTIME_MESSAGE_CAPACITY>);

impl RuntimeMessage {
    /// Records `message`, truncated at [`RUNTIME_MESSAGE_CAPACITY`].
    pub fn of(message: impl fmt::Display) -> Self {
        let mut text = Truncating(ArrayString::new());
        // `Truncating` never fails, so the message is recorded in full or cut
        // at capacity; neither outcome is an error worth propagating out of a
        // diagnostic.
        let _ = write!(text, "{message}");
        Self(text.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuntimeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A `fmt::Write` sink that stops at its capacity instead of failing, so a
/// diagnostic longer than the buffer is shortened rather than lost.
struct Truncating(ArrayString<RUNTIME_MESSAGE_CAPACITY>);

impl fmt::Write for Truncating {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            if self.0.try_push(character).is_err() {
                return Ok(());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProgramExecErrorDetail {
    #[error("child exit channel closed before completion")]
    ChildExitChannelDropped,
    #[error("child exit result was already consumed")]
    ChildExitAlreadyConsumed,
    #[error("compile hint is not allowed for precompiled artifacts")]
    HintNotAllowedForPrecompiledArtifact,
    #[error("artifact payload is missing")]
    MissingArtifactPayload,
    #[error("artifact is not executable by the selected runtime")]
    InvalidRuntimeArtifact,
    #[error("program entry point is missing or has an invalid type")]
    InvalidEntryPoint,
    #[error("program imports an unsupported host function")]
    UnsupportedImport,
    #[error("program path is invalid")]
    InvalidProgramPath,
    #[error("program path is not valid UTF-8")]
    InvalidProgramPathEncoding,
    #[error("program source is not granted by process authority")]
    ProgramSourceNotGranted,
    #[error("program artifact destination is not granted by process authority")]
    ProgramArtifactDestinationNotGranted,
    #[error("process authority rejected the requested grant")]
    ProcessAuthorityDenied,
    #[error("filesystem operation failed")]
    FilesystemOperationFailed,
    #[error("host filesystem service is unavailable")]
    HostFilesystemUnavailable,
    #[error("artifact signature verification failed")]
    ArtifactSignatureInvalid,
    #[error("artifact profile is unsupported")]
    ArtifactProfileInvalid,
    #[error("runtime operation failed: {0}")]
    RuntimeFailure(RuntimeMessage),
    #[error("program image replacement is not available for this runtime")]
    ImageReplacementUnavailable,
    #[error("program stack restoration is not available for this runtime")]
    StackRestoreUnavailable,
    #[error("program unwind export is missing or has an invalid type")]
    UnwindExportInvalid,
    #[error("program stack snapshot does not exist")]
    StackSnapshotMissing,
    #[error("program stack bounds are invalid")]
    StackBoundsInvalid,
    #[error("host operation failed")]
    HostOperationFailed,
    #[error("compiler plugin is unavailable")]
    CompilerUnavailable,
    #[error("compiler plugin artifact has the wrong shape")]
    CompilerPluginInvalid,
    #[error("compiler plugin memory contract is invalid")]
    CompilerMemoryContractInvalid,
    #[error("imported shared memory contract is invalid")]
    ImportedSharedMemoryContractInvalid,
    #[error("imported shared memory exceeds the user-memory budget")]
    ImportedSharedMemoryBudgetExceeded,
    #[error("compiler plugin ABI version mismatch")]
    CompilerAbiMismatch,
    #[error("compiler plugin rejected the input")]
    CompilerRejectedInput,
    #[error("compiler plugin allocation failed")]
    CompilerAllocationFailed,
    #[error("compiler plugin thread pointer overflowed")]
    CompilerThreadPointerOverflow,
    #[error("guest memory access overflowed")]
    GuestMemoryAccessOverflow,
    #[error("guest memory access is out of bounds")]
    GuestMemoryAccessOutOfBounds,
    #[error("guest memory value has an invalid type")]
    GuestMemoryTypeMismatch,
    #[error("internal invariant was violated")]
    InternalInvariant,
}

impl ProgramExecErrorDetail {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ChildExitChannelDropped => "child exit channel closed before completion",
            Self::ChildExitAlreadyConsumed => "child exit result was already consumed",
            Self::HintNotAllowedForPrecompiledArtifact => {
                "compile hint is not allowed for precompiled artifacts"
            }
            Self::MissingArtifactPayload => "artifact payload is missing",
            Self::InvalidRuntimeArtifact => "artifact is not executable by the selected runtime",
            Self::InvalidEntryPoint => "program entry point is missing or has an invalid type",
            Self::UnsupportedImport => "program imports an unsupported host function",
            Self::InvalidProgramPath => "program path is invalid",
            Self::InvalidProgramPathEncoding => "program path is not valid UTF-8",
            Self::ProgramSourceNotGranted => "program source is not granted by process authority",
            Self::ProgramArtifactDestinationNotGranted => {
                "program artifact destination is not granted by process authority"
            }
            Self::ProcessAuthorityDenied => "process authority rejected the requested grant",
            Self::FilesystemOperationFailed => "filesystem operation failed",
            Self::HostFilesystemUnavailable => "host filesystem service is unavailable",
            Self::ArtifactSignatureInvalid => "artifact signature verification failed",
            Self::ArtifactProfileInvalid => "artifact profile is unsupported",
            Self::RuntimeFailure(message) => message.as_str(),
            Self::ImageReplacementUnavailable => {
                "program image replacement is not available for this runtime"
            }
            Self::StackRestoreUnavailable => {
                "program stack restoration is not available for this runtime"
            }
            Self::UnwindExportInvalid => "program unwind export is missing or has an invalid type",
            Self::StackSnapshotMissing => "program stack snapshot does not exist",
            Self::StackBoundsInvalid => "program stack bounds are invalid",
            Self::HostOperationFailed => "host operation failed",
            Self::CompilerUnavailable => "compiler plugin is unavailable",
            Self::CompilerPluginInvalid => "compiler plugin artifact has the wrong shape",
            Self::CompilerMemoryContractInvalid => "compiler plugin memory contract is invalid",
            Self::ImportedSharedMemoryContractInvalid => {
                "imported shared memory contract is invalid"
            }
            Self::ImportedSharedMemoryBudgetExceeded => {
                "imported shared memory exceeds the user-memory budget"
            }
            Self::CompilerAbiMismatch => "compiler plugin ABI version mismatch",
            Self::CompilerRejectedInput => "compiler plugin rejected the input",
            Self::CompilerAllocationFailed => "compiler plugin allocation failed",
            Self::CompilerThreadPointerOverflow => "compiler plugin thread pointer overflowed",
            Self::GuestMemoryAccessOverflow => "guest memory access overflowed",
            Self::GuestMemoryAccessOutOfBounds => "guest memory access is out of bounds",
            Self::GuestMemoryTypeMismatch => "guest memory value has an invalid type",
            Self::InternalInvariant => "internal invariant was violated",
        }
    }
}

#[derive(Debug, Error)]
#[error("{kind}: {detail}")]
pub struct ProgramExecError {
    pub kind: ProgramExecErrorKind,
    pub detail: ProgramExecErrorDetail,
}

/// Every fallible program operation returns this by value, so its size is a
/// property of the whole program path rather than of this type alone. 128
/// bytes is `clippy::result_large_err`'s budget for an `Err` variant, and
/// [`RUNTIME_MESSAGE_CAPACITY`] is what has to give if the struct grows.
const _: () = assert!(
    size_of::<ProgramExecError>() <= 128,
    "ProgramExecError outgrew the budget an Err variant is allowed to carry"
);

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    /// The evidence gap #62 ran into: `RuntimeFailure` is the detail for
    /// failures the kernel has no name for, so the runtime's own sentence is
    /// the only account of them and has to survive the trip to the host.
    #[test]
    fn a_runtime_failure_carries_what_the_runtime_said() {
        let error = ProgramExecError {
            kind: ProgramExecErrorKind::Internal,
            detail: ProgramExecErrorDetail::RuntimeFailure(RuntimeMessage::of(
                "memory index 0 out of bounds",
            )),
        };

        assert_eq!(
            error.detail.as_str(),
            "memory index 0 out of bounds",
            "the host reads `detail.as_str()` verbatim onto the wire"
        );
        assert_eq!(
            error.to_string(),
            "the kernel rejected the program for an internal reason: \
             runtime operation failed: memory index 0 out of bounds"
        );
    }

    #[test]
    fn a_message_longer_than_the_buffer_is_shortened_rather_than_lost() {
        let long = "x".repeat(RUNTIME_MESSAGE_CAPACITY * 2);
        let message = RuntimeMessage::of(&long);

        assert_eq!(message.as_str().len(), RUNTIME_MESSAGE_CAPACITY);
        assert!(long.starts_with(message.as_str()));
    }

    #[test]
    fn a_detail_the_kernel_names_still_reads_as_its_own_sentence() {
        assert_eq!(
            ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded.as_str(),
            "imported shared memory exceeds the user-memory budget"
        );
    }
}
