//! Collecting the guest kernel's LLVM raw profile.
//!
//! An instrumented kernel (`vm --profile-generate`, `docs/pgo.md`) carries a
//! `.profraw` image in its own memory. This module walks it out over the
//! existing inspector RPC, writes the file, and hands it to `llvm-profdata
//! merge` — the same step a host program's profile takes before
//! `-C profile-use` can read it.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args as ClapArgs;
use helios_inspector_protocol::RpcError;
use helios_inspector_protocol::system::profiling::{
    self as system_profiling, ProfileSection, RawProfileError,
};

use crate::serial::RpcClient;

/// Why the guest kernel's LLVM raw profile could not be collected.
///
/// The guest's own refusals keep their typed shape in
/// [`GuestProfileRefusal`] rather than becoming text here: the message
/// an operator needs differs per refusal, and the refusal is what
/// chooses it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawProfileCollectError {
    #[error(
        "llvm-profdata is not on PATH, and the raw profile has to be merged before \
         -C profile-use can read it; `rustup component add llvm-tools` puts it in \
         $(rustc --print target-libdir)/../bin"
    )]
    ProfdataMissing,
    #[error("failed to ask the guest for the size of its LLVM raw profile: {source}")]
    AskSize {
        #[source]
        source: RpcError,
    },
    #[error("failed to read the LLVM raw profile at offset {offset} of {size}: {source}")]
    ReadWindow {
        offset: u64,
        size: u64,
        #[source]
        source: RpcError,
    },
    #[error("{source}")]
    GuestRefused {
        #[from]
        source: GuestProfileRefusal,
    },
    #[error("the guest reports a zero-byte LLVM raw profile")]
    EmptyProfile,
    #[error("the guest returned no bytes at offset {offset} of its {size}-byte raw profile")]
    EmptyWindow { offset: u64, size: u64 },
    #[error("failed to {step} {path}: {source}")]
    File {
        /// `create`, `write` or `flush`, the step that failed.
        step: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to run {tool}: {source}")]
    SpawnProfdata {
        tool: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{tool} merge exited with status {status}")]
    ProfdataFailed {
        tool: String,
        status: std::process::ExitStatus,
    },
    #[error("failed to report the collected profile: {source}")]
    Report {
        #[source]
        source: std::io::Error,
    },
}

/// The guest's typed refusal, rendered as the sentence the operator
/// needs.
///
/// It stays an error of its own so the guest's classification survives
/// the walk out: a kernel that carries no instrumentation and a kernel
/// whose writer disagrees with its toolchain are different problems with
/// different fixes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GuestProfileRefusal {
    #[error(
        "the running kernel carries no LLVM instrumentation; boot it with \
         `vm --profile-generate` to collect a profile"
    )]
    NotInstrumented,
    #[error(
        "the kernel's instrumentation reports raw profile version word {version:#018x}, \
         which its writer does not implement; the toolchain that built it and the one \
         docs/pgo.md pins have diverged"
    )]
    UnsupportedVersion { version: u64 },
    #[error("the kernel's {section} section is not a whole number of its records")]
    MalformedSection { section: &'static str },
    #[error("the profile is {len} bytes and the read started past its end")]
    OutOfRange { len: u64 },
    #[error("the guest serves at most {limit} bytes per read")]
    ReadTooLarge { limit: u32 },
}

impl From<RawProfileError> for GuestProfileRefusal {
    fn from(error: RawProfileError) -> Self {
        match error {
            RawProfileError::NotInstrumented => Self::NotInstrumented,
            RawProfileError::UnsupportedVersion(version) => Self::UnsupportedVersion { version },
            RawProfileError::MalformedSection(section) => Self::MalformedSection {
                section: match section {
                    ProfileSection::Counters => "__llvm_prf_cnts",
                    ProfileSection::Data => "__llvm_prf_data",
                    ProfileSection::Names => "__llvm_prf_names",
                },
            },
            RawProfileError::OutOfRange(len) => Self::OutOfRange { len },
            RawProfileError::ReadTooLarge(limit) => Self::ReadTooLarge { limit },
        }
    }
}

/// Bytes asked for per RPC.
///
/// The guest serves at most `helios_kernel::MAX_PROFILE_READ` in one call and
/// says so if asked for more; matching it keeps the walk to one round trip
/// per window.
const WINDOW_BYTES: u32 = 256 * 1024;

/// Writes the guest kernel's LLVM raw profile, then merges it.
#[derive(Debug, Clone, ClapArgs)]
pub(super) struct ProfileCommand {
    /// Path the raw profile is written to. The merged profile is written
    /// beside it with a `.profdata` extension unless `--profdata` names
    /// another path.
    pub(super) output: PathBuf,

    /// Path for the merged profile `-C profile-use` reads.
    #[arg(long)]
    pub(super) profdata: Option<PathBuf>,
}

impl ProfileCommand {
    fn profdata_path(&self) -> PathBuf {
        self.profdata
            .clone()
            .unwrap_or_else(|| self.output.with_extension("profdata"))
    }
}

/// Collects the profile the command asks for and merges it.
pub(super) async fn run(
    client: &RpcClient,
    command: &ProfileCommand,
) -> Result<(), RawProfileCollectError> {
    collect(client, &command.output, &command.profdata_path()).await
}

/// Collects into `raw`, then merges into `raw` with a `.profdata`
/// extension. This is the shape the bench actions' `--llvm-raw-profile-output`
/// takes, where the raw file names the run and the merged file is what a
/// later `-C profile-use` build consumes.
pub(super) async fn collect_beside(
    client: &RpcClient,
    raw: &Path,
) -> Result<(), RawProfileCollectError> {
    collect(client, raw, &raw.with_extension("profdata")).await
}

async fn collect(
    client: &RpcClient,
    raw: &Path,
    profdata: &Path,
) -> Result<(), RawProfileCollectError> {
    // The merge tool is looked up before the guest is asked for a byte: a
    // profile written to disk that nothing on this host can read is a
    // failure worth reporting before it is produced, not after.
    let profdata_tool = super::find_executable_in_path("llvm-profdata")
        .ok_or(RawProfileCollectError::ProfdataMissing)?;

    let size = system_profiling::raw_profile_size(client)
        .await
        .map_err(|source| RawProfileCollectError::AskSize { source })?
        .map_err(GuestProfileRefusal::from)?;
    if size == 0 {
        return Err(RawProfileCollectError::EmptyProfile);
    }

    let on_file = |step| {
        move |source| RawProfileCollectError::File {
            step,
            path: raw.display().to_string(),
            source,
        }
    };
    let mut file = File::create(raw).map_err(on_file("create"))?;
    let mut offset = 0u64;
    while offset < size {
        let window = system_profiling::raw_profile_read(client, offset, WINDOW_BYTES)
            .await
            .map_err(|source| RawProfileCollectError::ReadWindow {
                offset,
                size,
                source,
            })?
            .map_err(GuestProfileRefusal::from)?;
        if window.is_empty() {
            return Err(RawProfileCollectError::EmptyWindow { offset, size });
        }
        file.write_all(&window).map_err(on_file("write"))?;
        offset += window.len() as u64;
    }
    file.flush().map_err(on_file("flush"))?;

    let status = Command::new(&profdata_tool)
        .arg("merge")
        .arg("--output")
        .arg(profdata)
        .arg(raw)
        .status()
        .map_err(|source| RawProfileCollectError::SpawnProfdata {
            tool: profdata_tool.display().to_string(),
            source,
        })?;
    if !status.success() {
        return Err(RawProfileCollectError::ProfdataFailed {
            tool: profdata_tool.display().to_string(),
            status,
        });
    }

    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "llvm_raw_profile_output={} bytes={size}",
        raw.display()
    )
    .and_then(|()| writeln!(stderr, "llvm_profdata_output={}", profdata.display()))
    .map_err(|source| RawProfileCollectError::Report { source })?;
    Ok(())
}
