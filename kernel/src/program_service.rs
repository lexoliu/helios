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
    #[error("runtime operation failed")]
    RuntimeFailure,
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

#[derive(Debug, Error)]
#[error("{kind}: {detail}")]
pub struct ProgramExecError {
    pub kind: ProgramExecErrorKind,
    pub detail: ProgramExecErrorDetail,
}
