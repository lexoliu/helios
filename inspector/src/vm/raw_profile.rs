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

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Args as ClapArgs;
use helios_inspector_protocol::system::profiling::{self as system_profiling, RawProfileError};

use crate::serial::RpcClient;

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
pub(super) async fn run(client: &RpcClient, command: &ProfileCommand) -> Result<()> {
    collect(client, &command.output, &command.profdata_path()).await
}

/// Collects into `raw`, then merges into `raw` with a `.profdata`
/// extension. This is the shape the bench actions' `--llvm-raw-profile-output`
/// takes, where the raw file names the run and the merged file is what a
/// later `-C profile-use` build consumes.
pub(super) async fn collect_beside(client: &RpcClient, raw: &Path) -> Result<()> {
    collect(client, raw, &raw.with_extension("profdata")).await
}

async fn collect(client: &RpcClient, raw: &Path, profdata: &Path) -> Result<()> {
    // The merge tool is looked up before the guest is asked for a byte: a
    // profile written to disk that nothing on this host can read is a
    // failure worth reporting before it is produced, not after.
    let profdata_tool = super::find_executable_in_path("llvm-profdata").ok_or_else(|| {
        anyhow!(
            "llvm-profdata is not on PATH, and the raw profile has to be merged before \
             -C profile-use can read it; `rustup component add llvm-tools` puts it in \
             $(rustc --print target-libdir)/../bin"
        )
    })?;

    let size = system_profiling::raw_profile_size(client)
        .await
        .context("failed to ask the guest for the size of its LLVM raw profile")?
        .map_err(describe)?;
    if size == 0 {
        bail!("the guest reports a zero-byte LLVM raw profile");
    }

    let mut file =
        File::create(raw).with_context(|| format!("failed to create {}", raw.display()))?;
    let mut offset = 0u64;
    while offset < size {
        let window = system_profiling::raw_profile_read(client, offset, WINDOW_BYTES)
            .await
            .with_context(|| {
                format!("failed to read the LLVM raw profile at offset {offset} of {size}")
            })?
            .map_err(describe)?;
        if window.is_empty() {
            bail!("the guest returned no bytes at offset {offset} of its {size}-byte raw profile");
        }
        file.write_all(&window)
            .with_context(|| format!("failed to write {}", raw.display()))?;
        offset += window.len() as u64;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", raw.display()))?;

    let status = Command::new(&profdata_tool)
        .arg("merge")
        .arg("--output")
        .arg(profdata)
        .arg(raw)
        .status()
        .with_context(|| format!("failed to run {}", profdata_tool.display()))?;
    if !status.success() {
        bail!(
            "{} merge exited with status {status}",
            profdata_tool.display()
        );
    }

    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "llvm_raw_profile_output={} bytes={size}",
        raw.display()
    )?;
    writeln!(stderr, "llvm_profdata_output={}", profdata.display())?;
    Ok(())
}

/// Turns the guest's typed refusal into the message the operator needs.
fn describe(error: RawProfileError) -> anyhow::Error {
    use helios_inspector_protocol::system::profiling::ProfileSection;

    match error {
        RawProfileError::NotInstrumented => anyhow!(
            "the running kernel carries no LLVM instrumentation; boot it with \
             `vm --profile-generate` to collect a profile"
        ),
        RawProfileError::UnsupportedVersion(version) => anyhow!(
            "the kernel's instrumentation reports raw profile version word {version:#018x}, \
             which its writer does not implement; the toolchain that built it and the one \
             docs/pgo.md pins have diverged"
        ),
        RawProfileError::MalformedSection(section) => {
            let name = match section {
                ProfileSection::Counters => "__llvm_prf_cnts",
                ProfileSection::Data => "__llvm_prf_data",
                ProfileSection::Names => "__llvm_prf_names",
            };
            anyhow!("the kernel's {name} section is not a whole number of its records")
        }
        RawProfileError::OutOfRange(len) => {
            anyhow!("the profile is {len} bytes and the read started past its end")
        }
        RawProfileError::ReadTooLarge(limit) => {
            anyhow!("the guest serves at most {limit} bytes per read")
        }
    }
}
