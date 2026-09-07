//! Branch-hint feedback for the Helios AOT compiler plugin.
//!
//! Cranelift consumes no profile data. The one feedback channel the
//! vendored Wasmtime already reads is the wasm branch-hinting proposal:
//! `crates/cranelift/src/translate/code_translator.rs` marks the unlikely
//! successor of a hinted `if` or `br_if` cold and
//! `cranelift/codegen/src/machinst/blockorder.rs` lays cold blocks out of
//! line. This crate produces the `metadata.code.branch_hint` custom
//! section that drives it, from counts a real run recorded.
//!
//! The loop has three steps, one subcommand each:
//!
//! 1. [`instrument`] rewrites a module so every `if` and `br_if` records a
//!    taken/not-taken pair into a reserved region of its own linear memory,
//!    and dumps those counters to stdout when the program ends.
//! 2. [`profile::Profile::record`] turns one or more of those dumps, plus
//!    the site map the instrumenter wrote, into a profile.
//! 3. [`hint`] writes the hints the profile justifies back into the
//!    original, uninstrumented module.

pub mod component;
pub mod counts;
pub mod hint;
pub mod instrument;
pub mod module;
pub mod profile;
pub mod wasm;

/// Everything that can go wrong reading, rewriting or hinting a module.
///
/// The tool never continues past an input it does not understand: a hint
/// written from a misread offset is a silently wrong compilation, which is
/// far more expensive to find than a failed build.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Parse {
        context: &'static str,
        #[source]
        source: wasmparser::BinaryReaderError,
    },

    #[error("{0} is neither a core wasm module nor a component")]
    NotWasm(String),

    #[error(
        "expected exactly one core module importing `{IMPORT_MODULE}.{FD_WRITE}`, found {found}"
    )]
    CoreModuleAmbiguous { found: usize },

    #[error("the core module imports its memory; instrumentation needs a defined memory")]
    ImportedMemory,

    #[error("the core module defines no memory")]
    NoMemory,

    #[error("the core module defines more than one memory")]
    MultipleMemories,

    #[error("the core module does not import `{IMPORT_MODULE}.{name}`")]
    MissingImport { name: &'static str },

    #[error("the core module exports no `{ENTRY_EXPORT}` function to wrap")]
    MissingEntry,

    #[error("`{ENTRY_EXPORT}` exports function {func}, whose type is not `() -> ()`")]
    EntryNotNullary { func: u32 },

    #[error("function {func} has no body at index {func} in the code section")]
    MissingBody { func: u32 },

    #[error("type index {0} is out of range or is not a function type")]
    BadTypeIndex(u32),

    #[error("the core module has no code section")]
    NoCodeSection,

    #[error(
        "the profile records a branch at function {func} offset {offset}, where this module has \
         no `if` or `br_if`; the profile was recorded from a different build and must be re-recorded"
    )]
    StaleSite { func: u32, offset: u32 },

    #[error(
        "the profile was recorded from a code section fingerprinted {recorded}, this module's is {actual}; \
         re-record the profile against this build"
    )]
    StaleFingerprint { recorded: String, actual: String },

    #[error("the site map covers {sites} sites but the dump reports index {index}")]
    CountsOutOfRange { sites: usize, index: u64 },

    #[error("no `{BEGIN_MARKER}` marker in the recorded output")]
    NoProfileMarker,

    #[error("the recorded output ends without a `{END_MARKER}` marker; the guest died mid-dump")]
    TruncatedProfile,

    #[error("malformed branch-count record: {0:?}")]
    MalformedRecord(String),

    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

impl Error {
    pub(crate) fn parse(context: &'static str) -> impl Fn(wasmparser::BinaryReaderError) -> Self {
        move |source| Error::Parse { context, source }
    }

    pub fn io(path: impl AsRef<std::path::Path>) -> impl Fn(std::io::Error) -> Self {
        let path = path.as_ref().display().to_string();
        move |source| Error::Io {
            path: path.clone(),
            source,
        }
    }
}

/// The wasi module name whose `fd_write` carries the dump out of the guest
/// and whose `proc_exit` is the second place a program can end.
pub const IMPORT_MODULE: &str = "wasi_snapshot_preview1";
/// The import the dump writes through.
pub const FD_WRITE: &str = "fd_write";
/// The import a program that exits non-locally passes through.
pub const PROC_EXIT: &str = "proc_exit";
/// The core export the instrumenter wraps so the dump runs after the
/// program returns.
pub const ENTRY_EXPORT: &str = "_start";

/// First line of a counter dump.
pub const BEGIN_MARKER: &str = "!helios-branch-profile-1";
/// Last line of a counter dump. Its absence means the guest died before
/// the dump finished and the counts are not a profile.
pub const END_MARKER: &str = "!helios-branch-profile-end";

/// A site must be executed at least this many times before its ratio is
/// evidence rather than noise.
///
/// A hint moves the unlikely successor out of line, so a hint taken from
/// three executions can cost every later execution an extra jump for a
/// bias that was never measured. A thousand executions puts the binomial
/// 95% interval of an observed 90/10 split inside ±2 points, which is
/// narrow enough that [`HINT_RATIO`] separates a real bias from sampling.
pub const MIN_OBSERVATIONS: u64 = 1_000;

/// The share of executions one side must take before the site is hinted.
///
/// Cranelift's whole response to a hint is layout: the unlikely successor
/// leaves the straight-line path. At 90% the cost is bounded by one extra
/// jump on a tenth of the executions, against contiguous layout and denser
/// instruction cache lines on the other nine tenths. Below that the trade
/// stops being obviously positive, and a hint that is merely probable is
/// worse than none because Cranelift has no way to express "slightly".
pub const HINT_RATIO: f64 = 0.90;

/// Bytes of scratch the dump uses for its `fd_write` iovec and result.
pub const SCRATCH_BYTES: u32 = 16;

/// Bytes of output buffer the dump fills before flushing.
pub const BUFFER_BYTES: u32 = 64 * 1024;

/// Bytes of counter storage per instrumented branch site: two 64-bit
/// counters, taken first.
pub const COUNTER_BYTES: u32 = 16;

/// The kinds of branch Cranelift takes a hint for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchKind {
    If,
    BrIf,
}

impl BranchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BranchKind::If => "if",
            BranchKind::BrIf => "br_if",
        }
    }
}
