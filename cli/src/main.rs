use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use askama::Template;
use clap::{Parser, Subcommand, ValueEnum};
use ed25519_dalek::{SecretKey, SigningKey, VerifyingKey};
use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};
use helios_artifact::{TrailerError, cwasm_target_supports_wasm_simd, sign_payload_with_key};
use helios_compiler_support::{AotCompileHint, CompileError, precompile_artifact};
use helios_profdata::{
    FetchedProfile, KERNEL_PROFILE_ASSET, KernelProfileStore, KernelProfileStoreError,
    ProfileUseError, RELEASE_REPOSITORY,
};
use helios_workspace_root::{WorkspaceRoot, WorkspaceRootError};
use mbrman::{BOOT_ACTIVE, CHS, MBR, MBRPartitionEntry};
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use toml::Value;
use walkdir::WalkDir;
use wasmparser::Parser as WasmParser;
use wit_component::ComponentEncoder;

/// Why a `helios-cli` invocation did not do what it was asked.
///
/// One variant per subcommand: each owns the typed error of the work it
/// drives, so a failure names the command that produced it before it
/// names the step.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Aot(#[from] AotError),
    #[error("{0}")]
    CompilerPlugin(#[from] CompilerPluginError),
    #[error("{0}")]
    KernelPrebuild(#[from] PrebuildError),
    #[error("{0}")]
    LimineUefiImage(#[from] LimineError),
    #[error("{0}")]
    ProfileFetch(#[from] ProfileFetchError),
}

/// Why an ahead-of-time compile did not produce a signed artifact.
#[derive(Debug, thiserror::Error)]
enum AotError {
    #[error("{0}")]
    Key(#[from] KeyError),
    #[error("failed to read {path}: {source}")]
    ReadInput {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Compile(#[from] CompileError),
    #[error("failed to sign AOT payload: {source}")]
    Sign {
        #[source]
        source: TrailerError,
    },
    #[error("failed to write {path}: {source}")]
    WriteOutput {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Why the in-kernel compiler plugin could not be compiled and signed.
#[derive(Debug, thiserror::Error)]
enum CompilerPluginError {
    #[error("{COMPILER_PLUGIN_ROOT_KEY_ENV} is required: {source}")]
    MissingRootKey {
        #[source]
        source: std::env::VarError,
    },
    #[error("failed to decode compiler plugin root key: {source}")]
    DecodeRootKey {
        #[source]
        source: hex::FromHexError,
    },
    #[error("root key must be 32 bytes, got {len}")]
    RootKeyLength { len: usize },
    #[error("failed to read wasm from stdin: {source}")]
    ReadStdin {
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Compile(#[from] CompileError),
    #[error("failed to sign compiler plugin output: {source}")]
    Sign {
        #[source]
        source: TrailerError,
    },
    #[error("failed to write signed cwasm to stdout: {source}")]
    WriteStdout {
        #[source]
        source: io::Error,
    },
}

/// Why the root signing keypair could not be read or written.
#[derive(Debug, thiserror::Error)]
enum KeyError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("{path} does not contain a 32-byte Ed25519 secret key: {source}")]
    NotThirtyTwoBytes {
        path: String,
        #[source]
        source: std::array::TryFromSliceError,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("the operating system refused to supply entropy for a new root key: {source}")]
    Entropy {
        #[source]
        source: rand::rngs::SysError,
    },
}

/// Why the kernel-prebuild manifest and the bootfs it describes could
/// not be produced.
#[derive(Debug, thiserror::Error)]
enum PrebuildError {
    #[error("{0}")]
    WorkspaceRoot(#[from] WorkspaceRootError),
    #[error("{0}")]
    Key(#[from] KeyError),
    #[error("{0}")]
    BootPrograms(#[from] BootProgramError),
    #[error("{0}")]
    BootArtifacts(#[from] BootArtifactsError),
    #[error("{0}")]
    WasmBuild(#[from] WasmBuildError),
    #[error("{0}")]
    Bootfs(#[from] BootfsError),
    #[error("{0}")]
    Compile(#[from] CompileError),
    #[error("failed to create {path}: {source}")]
    CreateOutDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to sign init AOT payload: {source}")]
    SignInit {
        #[source]
        source: TrailerError,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to resolve {path}: {source}")]
    Resolve {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode the kernel-prebuild manifest: {source}")]
    EncodeManifest {
        #[source]
        source: serde_json::Error,
    },
}

/// Why one wasm program could not be built into an artifact.
#[derive(Debug, thiserror::Error)]
enum WasmBuildError {
    #[error("crate manifest {path} is missing")]
    ManifestMissing { path: String },
    #[error("failed to invoke cargo for {path}: {source}")]
    SpawnCargo {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("wasm build for {path} target {target} failed with status {status}")]
    BuildFailed {
        path: String,
        target: String,
        status: std::process::ExitStatus,
    },
    #[error("failed to resolve generated artifact {artifact}: {source}")]
    ResolveArtifact {
        artifact: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    /// wit-component reports through `anyhow`, which keeps no type to
    /// carry here, so its own report travels as the text it renders —
    /// the whole chain, not just its outermost line.
    #[error("failed to load core module {path}: {report}")]
    LoadCoreModule { path: String, report: String },
    #[error("failed to encode component {path}: {report}")]
    EncodeComponent { path: String, report: String },
}

/// Why the set of boot programs could not be resolved from the
/// workspace and the selection the caller made.
#[derive(Debug, thiserror::Error)]
enum BootProgramError {
    #[error("--boot-program must name at least one boot program")]
    EmptyFlagSelection,
    #[error("HELIOS_BOOT_PROGRAMS must name at least one boot program")]
    EmptyEnvSelection,
    #[error("HELIOS_BOOT_PROGRAMS referenced unknown program(s): {missing}")]
    UnknownPrograms { missing: String },
    #[error("failed to read {path}: {source}")]
    ReadProgramsRoot {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to read programs directory entry: {source}")]
    ReadProgramsEntry {
        #[source]
        source: io::Error,
    },
    #[error("program directory {path} has no valid UTF-8 name")]
    ProgramNameNotUtf8 { path: String },
    #[error("default program crate manifest {path} is missing")]
    ManifestMissing { path: String },
    #[error("failed to read {path}: {source}")]
    ReadManifest {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    ParseManifest {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path} is missing package.name or lib.name")]
    ManifestHasNoName { path: String },
}

/// Why the external boot-artifacts manifest does not describe artifacts
/// this build can use.
#[derive(Debug, thiserror::Error)]
enum BootArtifactsError {
    #[error("boot artifacts manifest {path} is missing")]
    ManifestMissing { path: String },
    #[error("failed to read boot artifacts manifest {path}: {source}")]
    ReadManifest {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse boot artifacts manifest {path}: {source}")]
    ParseManifest {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("boot artifact in {path} has an empty command")]
    EmptyCommand { path: String },
    #[error("boot artifact {command} in {path} has an empty {field}")]
    EmptyField {
        command: String,
        path: String,
        /// The manifest key that was empty, spelled the way the message
        /// reads it.
        field: &'static str,
    },
    #[error("boot artifact {command} must install under bin/")]
    BootfsPathNotUnderBin { command: String },
    #[error("boot artifact {command} source must be workspace-relative")]
    SourceNotRelative { command: String },
    #[error("boot artifact {command} support root must be workspace-relative")]
    SupportRootNotRelative { command: String },
    #[error(
        "boot artifact {command} support_root and support_bootfs_prefix must be specified together"
    )]
    SupportPairMismatch { command: String },
    #[error("boot artifact {command} support bootfs prefix must be relative")]
    SupportPrefixNotRelative { command: String },
    #[error("boot artifact {command} declares an empty target")]
    EmptyTarget { command: String },
    #[error("boot artifact support root {path} is missing")]
    SupportRootMissing { path: String },
    #[error("boot program command {command} is declared more than once")]
    DuplicateCommand { command: String },
    #[error("boot program(s) are not available for target {target}: {mismatches}")]
    TargetMismatch { target: String, mismatches: String },
}

/// Why one bootfs asset could not be produced.
#[derive(Debug, thiserror::Error)]
enum BootfsError {
    #[error("{0}")]
    WasmBuild(#[from] WasmBuildError),
    #[error("{0}")]
    Compile(#[from] CompileError),
    #[error("{0}")]
    BootArtifacts(#[from] BootArtifactsError),
    #[error("{0}")]
    BootPrograms(#[from] BootProgramError),
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to sign {payload} AOT payload: {source}")]
    Sign {
        /// Which payload was being signed, spelled the way the message
        /// reads it.
        payload: String,
        #[source]
        source: TrailerError,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to resolve {path}: {source}")]
    Resolve {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to walk support root {path}: {source}")]
    WalkSupportRoot {
        path: String,
        #[source]
        source: walkdir::Error,
    },
    #[error("failed to strip support root {root} from {source_path}: {source}")]
    StripSupportRoot {
        root: String,
        source_path: String,
        #[source]
        source: std::path::StripPrefixError,
    },
    #[error("{path} is not valid UTF-8")]
    PathNotUtf8 { path: String },
    #[error("failed to read directory {path}: {source}")]
    ReadDir {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Why the Limine UEFI disk image could not be built.
#[derive(Debug, thiserror::Error)]
enum LimineError {
    #[error("failed to canonicalize kernel {path}: {source}")]
    CanonicalizeKernel {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to find Limine; install `limine` or set HELIOS_LIMINE_BIN")]
    ToolchainMissing,
    #[error("failed to locate Limine shared files; set HELIOS_LIMINE_SHARE")]
    ShareDirMissing,
    #[error("Limine EFI bootloader is missing: {path}")]
    BootloaderMissing { path: String },
    #[error("failed to inspect {path}: {source}")]
    Inspect {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create image directory {path}: {source}")]
    CreateImageDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create Limine image {path}: {source}")]
    CreateImage {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to size Limine image {path}: {source}")]
    SizeImage {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create Limine image MBR: {source}")]
    CreateMbr {
        #[source]
        source: mbrman::Error,
    },
    #[error("Limine UEFI image is too large for an MBR partition table: {source}")]
    ImageTooLarge {
        #[source]
        source: std::num::TryFromIntError,
    },
    #[error("Limine UEFI image is too small for the FAT partition")]
    ImageTooSmall,
    #[error("failed to write Limine image MBR: {source}")]
    WriteMbr {
        #[source]
        source: mbrman::Error,
    },
    #[error("Limine partition offset exceeds image size")]
    PartitionOffsetTooLarge,
    #[error("failed to format Limine FAT32 partition: {source}")]
    FormatPartition {
        #[source]
        source: io::Error,
    },
    #[error("failed to {step}: {source}")]
    Partition {
        /// The partition operation that failed, spelled the way the
        /// message reads it.
        step: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to create {path}: {source}")]
    CreateFatEntry {
        /// The path inside the image, which is fixed by the layout.
        path: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to write {path}: {source}")]
    WriteFatEntry {
        path: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to render Limine configuration: {source}")]
    RenderConfig {
        #[source]
        source: askama::Error,
    },
    #[error("failed to open source file {path}: {source}")]
    OpenSource {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create FAT file {name}: {source}")]
    CreateFatFile {
        name: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to copy {path} into FAT file {name}: {source}")]
    CopyIntoFat {
        path: String,
        name: String,
        #[source]
        source: io::Error,
    },
}

const ROOT_SECRET_FILE: &str = "helios-root-secret.key";
const ROOT_PUBLIC_FILE: &str = "helios-root-public.key";
const PREBUILD_MANIFEST_FILE: &str = "kernel-prebuild.json";
const DEFAULT_INIT_ARGV0: &str = "/init.wasm";
const DEFAULT_BOOT_ARTIFACTS_MANIFEST: &str = "tools/wasi-apps/boot-artifacts.toml";
/// Why the kernel profile a release published did not reach the store.
///
/// A fetch either leaves a profile this toolchain can build against in
/// the store or fails saying which step did not answer: a release build
/// reads the store and nothing downstream can tell an empty store from a
/// half-written one.
#[derive(Debug, thiserror::Error)]
enum ProfileFetchError {
    #[error("{0}")]
    WorkspaceRoot(#[from] WorkspaceRootError),
    #[error(
        "{variable} holds characters a GitHub token does not: a token is [A-Za-z0-9_-], and \
         this one would be passed to curl as a header"
    )]
    Token { variable: &'static str },
    #[error("failed to run curl for {url}: {source}; the fetch downloads over HTTPS with curl")]
    Curl {
        url: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to hand curl its configuration for {url}: {source}")]
    CurlConfig {
        url: String,
        #[source]
        source: io::Error,
    },
    #[error("curl did not answer {url}: it exited {status} and said {stderr}")]
    CurlExited {
        url: String,
        status: String,
        stderr: String,
    },
    #[error(
        "{url} answered 404. Every release carries {KERNEL_PROFILE_ASSET} from release.yml's \
         kernel-profile job (docs/pgo.md); a release cut before that job existed gets its \
         assets by dispatching that workflow with its tag"
    )]
    NoRelease { url: String },
    #[error("{url} answered HTTP {status}: {body}")]
    HttpStatus {
        url: String,
        status: String,
        body: String,
    },
    #[error("curl wrote no HTTP status for {url}; it answered {len} bytes")]
    NoHttpStatus { url: String, len: usize },
    #[error("{url} did not answer with a GitHub release: {source}")]
    DecodeRelease {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "release {tag} of {repository} carries no {KERNEL_PROFILE_ASSET} asset; release.yml's \
         kernel-profile job attaches one to every release that carries a kernel \
         (docs/pgo.md), and an older release gets it by dispatching that workflow with \
         this tag"
    )]
    NoProfileAsset { repository: String, tag: String },
    #[error("failed to create {path}: {source}")]
    CreateDirectory {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Profile(#[from] ProfileUseError),
    #[error("{0}")]
    Store(#[from] KernelProfileStoreError),
}

const COMPILER_PLUGIN_BOOTFS_PATH: &str = "bin/compiler.cwasm";
const COMPILER_PLUGIN_ROOT_KEY_ENV: &str = "HELIOS_COMPILER_ROOT_KEY_HEX";
const COMPILER_PLUGIN_SHARED_MEMORY_MAX_BYTES: usize = 512 * 1024 * 1024;
const LIMINE_IMAGE_BYTES: u64 = 768 * 1024 * 1024;
const LIMINE_PARTITION_START_LBA: u32 = 2048;
const LIMINE_SECTOR_BYTES: u64 = 512;
const LIMINE_EFI_SYSTEM_PARTITION_TYPE: u8 = 0xef;
const LIMINE_DISK_SIGNATURE: [u8; 4] = *b"HELO";

#[derive(Template)]
#[template(path = "limine.conf", escape = "none")]
struct LimineConfigTemplate {
    baud: u32,
}

#[derive(Parser)]
#[command(name = "helios-cli")]
struct Cli {
    /// Directory holding the Cargo workspace manifest that repository-relative
    /// paths resolve against.
    ///
    /// Defaults to the nearest workspace root at or above the current
    /// directory, so the tool operates on the checkout it is run in rather
    /// than the one it was built in.
    #[arg(long, global = true, value_name = "PATH")]
    workspace_root: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Aot(AotCommand),
    CompilerPlugin(CompilerPluginCommand),
    KernelPrebuild(KernelPrebuildCommand),
    LimineUefiImage(LimineUefiImageCommand),
    ProfileFetch(ProfileFetchCommand),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Hint {
    Fast,
    Balanced,
    Performance,
}

impl From<Hint> for AotCompileHint {
    fn from(value: Hint) -> Self {
        match value {
            Hint::Fast => Self::Fast,
            Hint::Balanced => Self::Balanced,
            Hint::Performance => Self::Performance,
        }
    }
}

#[derive(Parser)]
struct AotCommand {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    target: String,
    #[arg(long, default_value = "performance")]
    hint: Hint,
    #[arg(long)]
    root_key: PathBuf,
}

#[derive(Parser)]
struct CompilerPluginCommand {
    #[arg(long)]
    target: String,
    #[arg(long, default_value = "performance")]
    hint: Hint,
}

#[derive(Parser)]
struct KernelPrebuildCommand {
    #[arg(long)]
    out_dir: PathBuf,
    #[arg(long)]
    target: String,
    #[arg(long)]
    profile: String,
    #[arg(long)]
    cargo: PathBuf,
    #[arg(long, default_value = "programs/init/Cargo.toml")]
    init_manifest: PathBuf,
    #[arg(long, default_value = "programs/init/bootfs")]
    bootfs_root: PathBuf,
    #[arg(long, default_value = DEFAULT_INIT_ARGV0)]
    init_argv0: String,
    #[arg(long = "boot-program")]
    boot_programs: Vec<String>,
    #[arg(long, default_value = DEFAULT_BOOT_ARTIFACTS_MANIFEST)]
    boot_artifacts_manifest: PathBuf,
    #[arg(long = "no-compiler-plugin", default_value_t = false)]
    no_compiler_plugin: bool,
}

#[derive(Parser)]
struct LimineUefiImageCommand {
    #[arg(long)]
    kernel: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    baud: u32,
    #[arg(long, value_enum)]
    efi_arch: LimineEfiArch,
}

/// Download the kernel profile a release published, so that a `--release`
/// x86-64 kernel is built the way the release's own kernel was
/// (`docs/pgo.md`).
#[derive(Parser)]
struct ProfileFetchCommand {
    /// Release to take the profile from. The latest release by default,
    /// which is the one every release build between releases spends.
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
    /// `owner/name` of the repository whose releases carry the profile.
    #[arg(long, value_name = "REPOSITORY", default_value = RELEASE_REPOSITORY)]
    repository: String,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LimineEfiArch {
    X86_64,
    Aarch64,
}

impl LimineEfiArch {
    /// Removable-media boot path the UEFI firmware looks for on this
    /// architecture, which is both what Limine installs and what the
    /// share directory is recognised by.
    fn efi_bootloader_name(self) -> &'static str {
        match self {
            Self::X86_64 => "BOOTX64.EFI",
            Self::Aarch64 => "BOOTAA64.EFI",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PrebuildManifest {
    target: String,
    init_component: PathBuf,
    init_argv0: String,
    bootfs_root: PathBuf,
    root_public_key: PathBuf,
    root_secret_key: PathBuf,
    bootfs_assets: Vec<BootAsset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BootAsset {
    path: String,
    source: PathBuf,
    kind: BootAssetKind,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum BootAssetKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Deserialize)]
struct BootArtifactsManifest {
    #[serde(default)]
    artifact: Vec<ExternalBootArtifact>,
}

#[derive(Clone, Debug, Deserialize)]
struct ExternalBootArtifact {
    command: String,
    package: String,
    version: String,
    source_url: String,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    requires_wasm_simd: bool,
    bootfs_path: String,
    source: PathBuf,
    support_root: Option<PathBuf>,
    support_bootfs_prefix: Option<String>,
}

/// The inputs every bootfs asset build shares.
///
/// They travel together because each asset is compiled by the same cargo, for
/// the same profile and target, signed by the same root key, and resolved
/// against the same checkout.
struct BootBuild<'a> {
    cargo: &'a Path,
    profile: &'a str,
    out_dir: &'a Path,
    target: &'a str,
    root_signing_key: &'a SigningKey,
    workspace_root: &'a WorkspaceRoot,
}

#[derive(Clone, Debug)]
struct ProgramManifest {
    command: String,
    manifest_path: PathBuf,
    artifact_name: String,
}

fn main() -> Result<(), CliError> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Aot(command) => Ok(run_aot(command)?),
        Commands::CompilerPlugin(command) => Ok(run_compiler_plugin(command)?),
        Commands::KernelPrebuild(command) => {
            Ok(run_kernel_prebuild(command, cli.workspace_root.as_deref())?)
        }
        Commands::LimineUefiImage(command) => Ok(run_limine_uefi_image(command)?),
        Commands::ProfileFetch(command) => {
            Ok(run_profile_fetch(command, cli.workspace_root.as_deref())?)
        }
    }
}

fn run_aot(command: AotCommand) -> Result<(), AotError> {
    let root_signing_key = read_signing_key(&command.root_key)?;
    let wasm = fs::read(&command.input).map_err(|source| AotError::ReadInput {
        path: command.input.display().to_string(),
        source,
    })?;
    let payload = precompile_artifact(&wasm, &command.target, command.hint.into())?.bytes;
    let signed = sign_payload_with_key(&payload, &root_signing_key)
        .map_err(|source| AotError::Sign { source })?;
    fs::write(&command.output, signed).map_err(|source| AotError::WriteOutput {
        path: command.output.display().to_string(),
        source,
    })?;
    Ok(())
}

fn run_compiler_plugin(command: CompilerPluginCommand) -> Result<(), CompilerPluginError> {
    let root_key_hex = std::env::var(COMPILER_PLUGIN_ROOT_KEY_ENV)
        .map_err(|source| CompilerPluginError::MissingRootKey { source })?;
    let root_key_bytes: [u8; 32] = hex::decode(root_key_hex.trim())
        .map_err(|source| CompilerPluginError::DecodeRootKey { source })?
        .try_into()
        .map_err(|bytes: Vec<u8>| CompilerPluginError::RootKeyLength { len: bytes.len() })?;
    let root_signing_key = SigningKey::from_bytes(&root_key_bytes);

    let mut wasm = Vec::new();
    io::stdin()
        .read_to_end(&mut wasm)
        .map_err(|source| CompilerPluginError::ReadStdin { source })?;
    let payload = precompile_artifact(&wasm, &command.target, command.hint.into())?.bytes;
    let signed = sign_payload_with_key(&payload, &root_signing_key)
        .map_err(|source| CompilerPluginError::Sign { source })?;
    io::stdout()
        .write_all(&signed)
        .map_err(|source| CompilerPluginError::WriteStdout { source })?;
    Ok(())
}

fn run_kernel_prebuild(
    command: KernelPrebuildCommand,
    explicit_workspace_root: Option<&Path>,
) -> Result<(), PrebuildError> {
    let workspace_root = WorkspaceRoot::resolve(explicit_workspace_root)?;
    fs::create_dir_all(&command.out_dir).map_err(|source| PrebuildError::CreateOutDir {
        path: command.out_dir.display().to_string(),
        source,
    })?;

    let root_secret_path = command.out_dir.join(ROOT_SECRET_FILE);
    let root_public_path = command.out_dir.join(ROOT_PUBLIC_FILE);
    let root_signing_key = ensure_root_keypair(&root_secret_path, &root_public_path)?;

    let init_manifest = workspace_root.join(&command.init_manifest);
    let bootfs_root = workspace_root.join(&command.bootfs_root);

    let selected_programs = selected_boot_programs(command.boot_programs)?;
    let boot_artifacts_manifest = workspace_root.join(&command.boot_artifacts_manifest);
    validate_external_boot_artifact_sources(
        &boot_artifacts_manifest,
        &command.target,
        &selected_programs,
        &workspace_root,
    )?;

    let init_component = build_component_program(
        &command.cargo,
        &command.profile,
        &command.out_dir,
        &init_manifest,
        "init-target",
        "helios_init.wasm",
    )?;
    let init_cwasm = command.out_dir.join("init_component.cwasm");
    let init_component_bytes = encode_component(&init_component)?;
    let init_payload = precompile_artifact(
        &init_component_bytes,
        &command.target,
        Hint::Performance.into(),
    )?
    .bytes;
    let init_signed = sign_payload_with_key(&init_payload, &root_signing_key)
        .map_err(|source| PrebuildError::SignInit { source })?;
    fs::write(&init_cwasm, init_signed).map_err(|source| PrebuildError::Write {
        path: init_cwasm.display().to_string(),
        source,
    })?;

    let build = BootBuild {
        cargo: &command.cargo,
        profile: &command.profile,
        out_dir: &command.out_dir,
        target: &command.target,
        root_signing_key: &root_signing_key,
        workspace_root: &workspace_root,
    };
    let mut bootfs_assets = Vec::new();
    if !command.no_compiler_plugin {
        bootfs_assets.push(build_compiler_plugin_asset(&build)?);
    }
    bootfs_assets.extend(build_boot_program_assets(
        &build,
        &selected_programs,
        &boot_artifacts_manifest,
    )?);

    let resolve = |path: &Path| {
        fs::canonicalize(path).map_err(|source| PrebuildError::Resolve {
            path: path.display().to_string(),
            source,
        })
    };
    let manifest = PrebuildManifest {
        target: command.target,
        init_component: resolve(&init_cwasm)?,
        init_argv0: command.init_argv0,
        bootfs_root: resolve(&bootfs_root)?,
        root_public_key: resolve(&root_public_path)?,
        root_secret_key: resolve(&root_secret_path)?,
        bootfs_assets,
    };
    let manifest_path = command.out_dir.join(PREBUILD_MANIFEST_FILE);
    let encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| PrebuildError::EncodeManifest { source })?;
    fs::write(&manifest_path, encoded).map_err(|source| PrebuildError::Write {
        path: manifest_path.display().to_string(),
        source,
    })?;
    Ok(())
}

fn run_limine_uefi_image(command: LimineUefiImageCommand) -> Result<(), LimineError> {
    let kernel =
        fs::canonicalize(&command.kernel).map_err(|source| LimineError::CanonicalizeKernel {
            path: command.kernel.display().to_string(),
            source,
        })?;
    let limine = LimineToolchain::discover(command.efi_arch)?;
    build_limine_uefi_image(&limine, &kernel, &command.output, command.baud)
}

struct LimineToolchain {
    efi_bootloader: PathBuf,
    efi_bootloader_name: &'static str,
}

impl LimineToolchain {
    fn discover(efi_arch: LimineEfiArch) -> Result<Self, LimineError> {
        let executable = std::env::var_os("HELIOS_LIMINE_BIN")
            .map(PathBuf::from)
            .or_else(|| find_executable_in_path("limine"))
            .ok_or(LimineError::ToolchainMissing)?;
        let share_dir = std::env::var_os("HELIOS_LIMINE_SHARE")
            .map(PathBuf::from)
            .or_else(|| limine_datadir(&executable))
            .or_else(|| infer_limine_share_dir(&executable, efi_arch))
            .ok_or(LimineError::ShareDirMissing)?;
        let efi_bootloader_name = efi_arch.efi_bootloader_name();
        let efi_bootloader = share_dir.join(efi_bootloader_name);
        if !efi_bootloader.is_file() {
            return Err(LimineError::BootloaderMissing {
                path: efi_bootloader.display().to_string(),
            });
        }
        Ok(Self {
            efi_bootloader,
            efi_bootloader_name,
        })
    }
}

/// Asks the Limine executable where its shared files live.
///
/// Every failure means the same thing to the caller — this executable
/// cannot say — and [`LimineToolchain::discover`] goes on to the next
/// way of finding the directory, so the reason is not carried further.
fn limine_datadir(executable: &Path) -> Option<PathBuf> {
    let output = Command::new(executable)
        .arg("--print-datadir")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    Some(PathBuf::from(path.trim()))
}

fn build_limine_uefi_image(
    limine: &LimineToolchain,
    kernel: &Path,
    image: &Path,
    baud: u32,
) -> Result<(), LimineError> {
    let image_bytes = limine_image_bytes(kernel, &limine.efi_bootloader)?;
    if let Some(parent) = image.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|source| LimineError::CreateImageDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let mut image_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(image)
        .map_err(|source| LimineError::CreateImage {
            path: image.display().to_string(),
            source,
        })?;
    image_file
        .set_len(image_bytes)
        .map_err(|source| LimineError::SizeImage {
            path: image.display().to_string(),
            source,
        })?;
    write_limine_mbr(&mut image_file, image_bytes)?;
    write_limine_fat_volume(&mut image_file, image_bytes, kernel, limine, baud)
}

fn limine_image_bytes(kernel: &Path, efi_bootloader: &Path) -> Result<u64, LimineError> {
    let inspect = |path: &Path| {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(|source| LimineError::Inspect {
                path: path.display().to_string(),
                source,
            })
    };
    let payload_bytes = inspect(kernel)? + inspect(efi_bootloader)?;
    Ok(LIMINE_IMAGE_BYTES.max(payload_bytes + 128 * 1024 * 1024))
}

fn write_limine_mbr(image: &mut fs::File, image_bytes: u64) -> Result<(), LimineError> {
    let mut mbr = MBR::new_from(image, LIMINE_SECTOR_BYTES as u32, LIMINE_DISK_SIGNATURE)
        .map_err(|source| LimineError::CreateMbr { source })?;
    let total_sectors = u32::try_from(image_bytes / LIMINE_SECTOR_BYTES)
        .map_err(|source| LimineError::ImageTooLarge { source })?;
    let partition_sectors = total_sectors
        .checked_sub(LIMINE_PARTITION_START_LBA)
        .ok_or(LimineError::ImageTooSmall)?;
    mbr[1] = MBRPartitionEntry {
        boot: BOOT_ACTIVE,
        first_chs: CHS::empty(),
        sys: LIMINE_EFI_SYSTEM_PARTITION_TYPE,
        last_chs: CHS::empty(),
        starting_lba: LIMINE_PARTITION_START_LBA,
        sectors: partition_sectors,
    };
    mbr.write_into(image)
        .map_err(|source| LimineError::WriteMbr { source })
}

fn write_limine_fat_volume(
    image: &mut fs::File,
    image_bytes: u64,
    kernel: &Path,
    limine: &LimineToolchain,
    baud: u32,
) -> Result<(), LimineError> {
    let partition_offset = u64::from(LIMINE_PARTITION_START_LBA) * LIMINE_SECTOR_BYTES;
    let partition_len = image_bytes
        .checked_sub(partition_offset)
        .ok_or(LimineError::PartitionOffsetTooLarge)?;
    let mut partition = FileSlice::new(image, partition_offset, partition_len);
    fatfs::format_volume(
        &mut partition,
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .volume_label(*b"HELIOS     "),
    )
    .map_err(|source| LimineError::FormatPartition { source })?;
    write_fat32_hidden_sectors(&mut partition, LIMINE_PARTITION_START_LBA)?;
    partition
        .seek(SeekFrom::Start(0))
        .map_err(|source| LimineError::Partition {
            step: "rewind Limine FAT32 partition",
            source,
        })?;
    let fs =
        FileSystem::new(partition, FsOptions::new()).map_err(|source| LimineError::Partition {
            step: "open Limine FAT32 partition",
            source,
        })?;
    let root = fs.root_dir();
    fn create_dir<'a, IO: fatfs::ReadWriteSeek>(
        parent: &fatfs::Dir<'a, IO>,
        name: &str,
        path: &'static str,
    ) -> Result<fatfs::Dir<'a, IO>, LimineError> {
        parent
            .create_dir(name)
            .map_err(|source| LimineError::CreateFatEntry { path, source })
    }
    let boot = create_dir(&root, "boot", "/boot")?;
    let efi = create_dir(&root, "EFI", "/EFI")?;
    let efi_boot = create_dir(&efi, "BOOT", "/EFI/BOOT")?;
    let limine_dir = create_dir(&boot, "limine", "/boot/limine")?;
    write_file_to_fat(
        &efi_boot,
        limine.efi_bootloader_name,
        &limine.efi_bootloader,
    )?;
    let config = LimineConfigTemplate { baud }
        .render()
        .map_err(|source| LimineError::RenderConfig { source })?;
    for (directory, path) in [
        (&limine_dir, "/boot/limine/limine.conf"),
        (&efi_boot, "/EFI/BOOT/limine.conf"),
    ] {
        let mut file = directory
            .create_file("limine.conf")
            .map_err(|source| LimineError::CreateFatEntry { path, source })?;
        file.write_all(config.as_bytes())
            .map_err(|source| LimineError::WriteFatEntry { path, source })?;
    }
    write_file_to_fat(&boot, "helios", kernel)?;
    Ok(())
}

fn write_fat32_hidden_sectors(
    partition: &mut FileSlice<'_>,
    hidden_sectors: u32,
) -> Result<(), LimineError> {
    const BPB_HIDDEN_SECTORS_OFFSET: u64 = 0x1c;
    const FAT32_BACKUP_BOOT_SECTOR: u64 = 6 * LIMINE_SECTOR_BYTES;
    for boot_sector in [0, FAT32_BACKUP_BOOT_SECTOR] {
        partition
            .seek(SeekFrom::Start(boot_sector + BPB_HIDDEN_SECTORS_OFFSET))
            .map_err(|source| LimineError::Partition {
                step: "seek to FAT32 hidden-sectors field",
                source,
            })?;
        partition
            .write_all(&hidden_sectors.to_le_bytes())
            .map_err(|source| LimineError::Partition {
                step: "write FAT32 hidden-sectors field",
                source,
            })?;
    }
    Ok(())
}

fn write_file_to_fat<IO>(
    root: &fatfs::Dir<'_, IO>,
    name: &str,
    source: &Path,
) -> Result<(), LimineError>
where
    IO: fatfs::ReadWriteSeek,
{
    let mut input = fs::File::open(source).map_err(|error| LimineError::OpenSource {
        path: source.display().to_string(),
        source: error,
    })?;
    let mut output = root
        .create_file(name)
        .map_err(|error| LimineError::CreateFatFile {
            name: name.to_owned(),
            source: io::Error::other(error),
        })?;
    std::io::copy(&mut input, &mut output).map_err(|error| LimineError::CopyIntoFat {
        path: source.display().to_string(),
        name: name.to_owned(),
        source: error,
    })?;
    Ok(())
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|path| path.join(name))
        .find(|candidate| candidate.is_file())
}

fn infer_limine_share_dir(executable: &Path, efi_arch: LimineEfiArch) -> Option<PathBuf> {
    let bin_dir = executable.parent()?;
    let prefix_dir = bin_dir.parent()?;
    let efi_bootloader_name = efi_arch.efi_bootloader_name();
    [
        prefix_dir.join("share").join("limine"),
        prefix_dir.join("share"),
    ]
    .into_iter()
    .find(|path| path.join(efi_bootloader_name).is_file())
}

struct FileSlice<'a> {
    file: &'a mut fs::File,
    offset: u64,
    len: u64,
    position: u64,
}

impl<'a> FileSlice<'a> {
    fn new(file: &'a mut fs::File, offset: u64, len: u64) -> Self {
        Self {
            file,
            offset,
            len,
            position: 0,
        }
    }
}

impl Read for FileSlice<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.len {
            return Ok(0);
        }
        let remaining = self.len - self.position;
        let count = remaining.min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(
            self.offset
                .checked_add(self.position)
                .ok_or_else(|| std::io::Error::other("file slice position overflow"))?,
        ))?;
        let read = self.file.read(&mut buf[..count])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Write for FileSlice<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.position >= self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "file slice write exceeds partition bounds",
            ));
        }
        let remaining = self.len - self.position;
        let count = remaining.min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(
            self.offset
                .checked_add(self.position)
                .ok_or_else(|| std::io::Error::other("file slice position overflow"))?,
        ))?;
        let written = self.file.write(&buf[..count])?;
        self.position += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for FileSlice<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let next = match pos {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        if !(0..=i128::from(self.len)).contains(&next) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file slice seek is outside partition bounds",
            ));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

fn build_compiler_plugin_asset(build: &BootBuild<'_>) -> Result<BootAsset, BootfsError> {
    let wasm_path = build_wasm_program(
        build.cargo,
        build.profile,
        build.out_dir,
        &build
            .workspace_root
            .join(Path::new("compiler-plugin/Cargo.toml")),
        "compiler-plugin-target",
        "wasm32-wasip1-threads",
        "helios_compiler_plugin.wasm",
    )?;
    let wasm = fs::read(&wasm_path).map_err(|source| BootfsError::Read {
        path: wasm_path.display().to_string(),
        source,
    })?;
    let payload = precompile_artifact(&wasm, build.target, Hint::Performance.into())?.bytes;
    let signed = sign_payload_with_key(&payload, build.root_signing_key).map_err(|source| {
        BootfsError::Sign {
            payload: "compiler plugin".to_owned(),
            source,
        }
    })?;
    let output_path = build.out_dir.join("compiler_plugin.cwasm");
    write_bootfs_artifact(&output_path, signed)?;

    Ok(BootAsset {
        path: COMPILER_PLUGIN_BOOTFS_PATH.to_owned(),
        source: resolve_bootfs_source(&output_path)?,
        kind: BootAssetKind::File,
    })
}

/// Writes one signed bootfs artifact to `path`.
fn write_bootfs_artifact(path: &Path, bytes: Vec<u8>) -> Result<(), BootfsError> {
    fs::write(path, bytes).map_err(|source| BootfsError::Write {
        path: path.display().to_string(),
        source,
    })
}

/// The absolute path the manifest records for one bootfs asset.
fn resolve_bootfs_source(path: &Path) -> Result<PathBuf, BootfsError> {
    fs::canonicalize(path).map_err(|source| BootfsError::Resolve {
        path: path.display().to_string(),
        source,
    })
}

fn selected_boot_programs(
    boot_programs: Vec<String>,
) -> Result<Option<BTreeSet<String>>, BootProgramError> {
    if !boot_programs.is_empty() {
        let selected = boot_programs.into_iter().collect::<BTreeSet<_>>();
        if selected.is_empty() {
            return Err(BootProgramError::EmptyFlagSelection);
        }
        return Ok(Some(selected));
    }

    let Some(raw) = std::env::var_os("HELIOS_BOOT_PROGRAMS") else {
        return Ok(None);
    };
    let selected = raw
        .to_string_lossy()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if selected.is_empty() {
        return Err(BootProgramError::EmptyEnvSelection);
    }
    Ok(Some(selected))
}

fn build_boot_program_assets(
    build: &BootBuild<'_>,
    selected_programs: &Option<BTreeSet<String>>,
    boot_artifacts_manifest: &Path,
) -> Result<Vec<BootAsset>, BootfsError> {
    let programs_root = build.workspace_root.join(Path::new("programs"));
    let mut available_programs = BTreeSet::new();
    let mut manifests = Vec::new();
    let mut external_artifacts = read_external_boot_artifacts(boot_artifacts_manifest)?;

    for entry in
        fs::read_dir(&programs_root).map_err(|source| BootProgramError::ReadProgramsRoot {
            path: programs_root.display().to_string(),
            source,
        })?
    {
        let entry = entry.map_err(|source| BootProgramError::ReadProgramsEntry { source })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(command) = path.file_name().and_then(|name| name.to_str()) else {
            return Err(BootProgramError::ProgramNameNotUtf8 {
                path: path.display().to_string(),
            }
            .into());
        };
        if command == "init" {
            continue;
        }
        available_programs.insert(command.to_owned());
        if selected_programs
            .as_ref()
            .is_some_and(|selected| !selected.contains(command))
        {
            continue;
        }
        manifests.push(read_program_manifest(command, &path.join("Cargo.toml"))?);
    }
    for artifact in &external_artifacts {
        if !available_programs.insert(artifact.command.clone()) {
            return Err(BootArtifactsError::DuplicateCommand {
                command: artifact.command.clone(),
            }
            .into());
        }
    }
    reject_selected_target_mismatches(&external_artifacts, build.target, selected_programs)?;
    external_artifacts.retain(|artifact| {
        artifact.supports_target(build.target)
            && selected_programs
                .as_ref()
                .is_none_or(|selected| selected.contains(&artifact.command))
    });

    if let Some(selected_programs) = selected_programs {
        let missing_programs = selected_programs
            .iter()
            .filter(|command| !available_programs.contains(*command))
            .cloned()
            .collect::<Vec<_>>();
        if !missing_programs.is_empty() {
            return Err(BootProgramError::UnknownPrograms {
                missing: missing_programs.join(", "),
            }
            .into());
        }
    }

    manifests.sort_by(|left, right| left.command.cmp(&right.command));
    external_artifacts.sort_by(|left, right| left.command.cmp(&right.command));
    let mut assets = manifests
        .into_iter()
        .map(|manifest| build_boot_program_asset(build, manifest))
        .collect::<Result<Vec<_>, BootfsError>>()?;
    for artifact in external_artifacts {
        assets.extend(build_external_boot_artifact_assets(build, artifact)?);
    }
    Ok(assets)
}

fn read_external_boot_artifacts(
    path: &Path,
) -> Result<Vec<ExternalBootArtifact>, BootArtifactsError> {
    if !path.is_file() {
        return Err(BootArtifactsError::ManifestMissing {
            path: path.display().to_string(),
        });
    }
    let manifest = fs::read_to_string(path).map_err(|source| BootArtifactsError::ReadManifest {
        path: path.display().to_string(),
        source,
    })?;
    let manifest = toml::from_str::<BootArtifactsManifest>(&manifest).map_err(|source| {
        BootArtifactsError::ParseManifest {
            path: path.display().to_string(),
            source,
        }
    })?;
    for artifact in &manifest.artifact {
        if artifact.command.is_empty() {
            return Err(BootArtifactsError::EmptyCommand {
                path: path.display().to_string(),
            });
        }
        let empty = |field| BootArtifactsError::EmptyField {
            command: artifact.command.clone(),
            path: path.display().to_string(),
            field,
        };
        for (value, field) in [
            (&artifact.package, "package"),
            (&artifact.version, "version"),
            (&artifact.source_url, "source URL"),
            (&artifact.bootfs_path, "bootfs path"),
        ] {
            if value.is_empty() {
                return Err(empty(field));
            }
        }
        if !artifact.bootfs_path.starts_with("bin/") {
            return Err(BootArtifactsError::BootfsPathNotUnderBin {
                command: artifact.command.clone(),
            });
        }
        if !artifact.source.is_relative() {
            return Err(BootArtifactsError::SourceNotRelative {
                command: artifact.command.clone(),
            });
        }
        if let Some(support_root) = &artifact.support_root
            && !support_root.is_relative()
        {
            return Err(BootArtifactsError::SupportRootNotRelative {
                command: artifact.command.clone(),
            });
        }
        if artifact.support_root.is_some() != artifact.support_bootfs_prefix.is_some() {
            return Err(BootArtifactsError::SupportPairMismatch {
                command: artifact.command.clone(),
            });
        }
        if let Some(prefix) = &artifact.support_bootfs_prefix
            && (prefix.is_empty() || prefix.starts_with('/'))
        {
            return Err(BootArtifactsError::SupportPrefixNotRelative {
                command: artifact.command.clone(),
            });
        }
        if artifact.targets.iter().any(String::is_empty) {
            return Err(BootArtifactsError::EmptyTarget {
                command: artifact.command.clone(),
            });
        }
    }
    Ok(manifest.artifact)
}

fn validate_external_boot_artifact_sources(
    path: &Path,
    target: &str,
    selected_programs: &Option<BTreeSet<String>>,
    workspace_root: &WorkspaceRoot,
) -> Result<(), BootArtifactsError> {
    for artifact in read_external_boot_artifacts(path)? {
        if !artifact.supports_target(target) {
            continue;
        }
        if selected_programs
            .as_ref()
            .is_some_and(|selected| !selected.contains(&artifact.command))
        {
            continue;
        }
        if let Some(support_root) = &artifact.support_root {
            let support_root = workspace_root.join(support_root);
            if !support_root.is_dir() {
                return Err(BootArtifactsError::SupportRootMissing {
                    path: support_root.display().to_string(),
                });
            }
        }
    }
    Ok(())
}

fn reject_selected_target_mismatches(
    artifacts: &[ExternalBootArtifact],
    target: &str,
    selected_programs: &Option<BTreeSet<String>>,
) -> Result<(), BootArtifactsError> {
    let Some(selected_programs) = selected_programs else {
        return Ok(());
    };
    let mismatches = artifacts
        .iter()
        .filter(|artifact| selected_programs.contains(&artifact.command))
        .filter(|artifact| !artifact.supports_target(target))
        .map(|artifact| format!("{} {}", artifact.command, artifact.target_constraints()))
        .collect::<Vec<_>>();
    if !mismatches.is_empty() {
        return Err(BootArtifactsError::TargetMismatch {
            target: target.to_owned(),
            mismatches: mismatches.join("; "),
        });
    }
    Ok(())
}

impl ExternalBootArtifact {
    fn supports_target(&self, target: &str) -> bool {
        let target_allowed =
            self.targets.is_empty() || self.targets.iter().any(|candidate| candidate == target);
        let simd_satisfied = !self.requires_wasm_simd || cwasm_target_supports_wasm_simd(target);
        target_allowed && simd_satisfied
    }

    fn target_constraints(&self) -> String {
        let mut constraints = Vec::new();
        if !self.targets.is_empty() {
            constraints.push(format!("supports {}", self.targets.join(", ")));
        }
        if self.requires_wasm_simd {
            constraints.push("requires wasm SIMD".to_owned());
        }
        constraints.join(" and ")
    }
}

fn build_boot_program_asset(
    build: &BootBuild<'_>,
    manifest: ProgramManifest,
) -> Result<BootAsset, BootfsError> {
    let wasm_path = build_component_program(
        build.cargo,
        build.profile,
        build.out_dir,
        &manifest.manifest_path,
        &format!("bootfs-{}-target", manifest.command),
        &manifest.artifact_name,
    )?;
    let component_bytes = encode_component(&wasm_path)?;
    let payload =
        precompile_artifact(&component_bytes, build.target, Hint::Performance.into())?.bytes;
    let signed = sign_payload_with_key(&payload, build.root_signing_key).map_err(|source| {
        BootfsError::Sign {
            payload: "bootfs".to_owned(),
            source,
        }
    })?;
    let output_path = build
        .out_dir
        .join(format!("{}_bootfs_component.cwasm", manifest.command));
    write_bootfs_artifact(&output_path, signed)?;

    Ok(BootAsset {
        path: format!("bin/{}", manifest.command),
        source: resolve_bootfs_source(&output_path)?,
        kind: BootAssetKind::File,
    })
}

fn build_external_boot_artifact_assets(
    build: &BootBuild<'_>,
    artifact: ExternalBootArtifact,
) -> Result<Vec<BootAsset>, BootfsError> {
    let source = build.workspace_root.join(&artifact.source);
    let wasm = fs::read(&source).map_err(|error| BootfsError::Read {
        path: source.display().to_string(),
        source: error,
    })?;
    let payload = precompile_artifact(&wasm, build.target, Hint::Performance.into())?.bytes;
    let signed = sign_payload_with_key(&payload, build.root_signing_key).map_err(|source| {
        BootfsError::Sign {
            payload: format!("external bootfs {}", artifact.command),
            source,
        }
    })?;
    let output_path = build
        .out_dir
        .join(format!("{}_bootfs_component.cwasm", artifact.command));
    write_bootfs_artifact(&output_path, signed)?;

    let mut assets = vec![BootAsset {
        path: artifact.bootfs_path,
        source: resolve_bootfs_source(&output_path)?,
        kind: BootAssetKind::File,
    }];
    if let (Some(support_root), Some(prefix)) =
        (&artifact.support_root, &artifact.support_bootfs_prefix)
    {
        assets.extend(build_external_support_assets(
            &build.workspace_root.join(support_root),
            prefix,
        )?);
    }
    Ok(assets)
}

fn build_external_support_assets(
    root: &Path,
    bootfs_prefix: &str,
) -> Result<Vec<BootAsset>, BootfsError> {
    if !root.is_dir() {
        return Err(BootArtifactsError::SupportRootMissing {
            path: root.display().to_string(),
        }
        .into());
    }
    let mut assets = Vec::new();
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.map_err(|source| BootfsError::WalkSupportRoot {
            path: root.display().to_string(),
            source,
        })?;
        if entry.path() == root {
            continue;
        }
        let source = entry.into_path();
        let relative = source
            .strip_prefix(root)
            .map_err(|error| BootfsError::StripSupportRoot {
                root: root.display().to_string(),
                source_path: source.display().to_string(),
                source: error,
            })?
            .to_str()
            .ok_or_else(|| BootfsError::PathNotUtf8 {
                path: source.display().to_string(),
            })?
            .replace('\\', "/");
        let kind = if source.is_dir() {
            if !is_empty_directory(&source)? {
                continue;
            }
            BootAssetKind::Directory
        } else if source.is_file() {
            BootAssetKind::File
        } else {
            continue;
        };
        assets.push(BootAsset {
            path: format!("{}/{}", bootfs_prefix.trim_end_matches('/'), relative),
            source: resolve_bootfs_source(&source)?,
            kind,
        });
    }
    Ok(assets)
}

fn is_empty_directory(path: &Path) -> Result<bool, BootfsError> {
    Ok(fs::read_dir(path)
        .map_err(|source| BootfsError::ReadDir {
            path: path.display().to_string(),
            source,
        })?
        .next()
        .is_none())
}

fn read_program_manifest(
    command: &str,
    manifest_path: &Path,
) -> Result<ProgramManifest, BootProgramError> {
    if !manifest_path.is_file() {
        return Err(BootProgramError::ManifestMissing {
            path: manifest_path.display().to_string(),
        });
    }
    let manifest =
        fs::read_to_string(manifest_path).map_err(|source| BootProgramError::ReadManifest {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let manifest = manifest
        .parse::<Value>()
        .map_err(|source| BootProgramError::ParseManifest {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let artifact_stem = manifest
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(Value::as_str)
                .map(|name| name.replace('-', "_"))
        })
        .ok_or_else(|| BootProgramError::ManifestHasNoName {
            path: manifest_path.display().to_string(),
        })?;
    Ok(ProgramManifest {
        command: command.to_owned(),
        manifest_path: manifest_path.to_path_buf(),
        artifact_name: format!("{artifact_stem}.wasm"),
    })
}

fn build_component_program(
    cargo: &Path,
    profile: &str,
    out_dir: &Path,
    manifest_path: &Path,
    target_dir_name: &str,
    artifact_name: &str,
) -> Result<PathBuf, WasmBuildError> {
    build_wasm_program(
        cargo,
        profile,
        out_dir,
        manifest_path,
        target_dir_name,
        "wasm32-wasip2",
        artifact_name,
    )
}

fn build_wasm_program(
    cargo: &Path,
    profile: &str,
    out_dir: &Path,
    manifest_path: &Path,
    target_dir_name: &str,
    target_triple: &str,
    artifact_name: &str,
) -> Result<PathBuf, WasmBuildError> {
    if !manifest_path.is_file() {
        return Err(WasmBuildError::ManifestMissing {
            path: manifest_path.display().to_string(),
        });
    }
    let target_dir = out_dir.join(target_dir_name);
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--target")
        .arg(target_triple)
        .arg("--target-dir")
        .arg(&target_dir);
    if profile == "release" {
        command.arg("--release");
        if target_triple == "wasm32-wasip1-threads" {
            command.env("CARGO_PROFILE_RELEASE_LTO", "fat");
            command.env("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1");
        }
    } else {
        command.env("CARGO_PROFILE_DEV_OPT_LEVEL", "z");
        command.env("CARGO_PROFILE_DEV_DEBUG", "0");
        command.env("CARGO_PROFILE_DEV_CODEGEN_UNITS", "1");
        command.env("CARGO_PROFILE_DEV_PANIC", "abort");
    }
    command.env_remove("CARGO_ENCODED_RUSTFLAGS");
    command.env("RUSTFLAGS", wasm_rustflags(target_triple));
    let status = command
        .status()
        .map_err(|source| WasmBuildError::SpawnCargo {
            path: manifest_path.display().to_string(),
            source,
        })?;
    if !status.success() {
        return Err(WasmBuildError::BuildFailed {
            path: manifest_path.display().to_string(),
            target: target_triple.to_owned(),
            status,
        });
    }

    let profile_dir = if profile == "release" {
        "release"
    } else {
        "debug"
    };
    fs::canonicalize(
        target_dir
            .join(target_triple)
            .join(profile_dir)
            .join(artifact_name),
    )
    .map_err(|source| WasmBuildError::ResolveArtifact {
        artifact: artifact_name.to_owned(),
        source,
    })
}

fn wasm_rustflags(target_triple: &str) -> String {
    match target_triple {
        "wasm32-wasip1-threads" => format!(
            "-C debuginfo=0 -C strip=symbols -C link-arg=--no-entry -C link-arg=--export=__tls_base -C link-arg=--max-memory={COMPILER_PLUGIN_SHARED_MEMORY_MAX_BYTES}"
        ),
        _ => "-C debuginfo=0 -C strip=debuginfo".to_owned(),
    }
}

/// Encodes one core module as a component, or passes a component through.
///
/// `wit-component` reports through `anyhow`, which leaves no error type
/// to keep, so its report is captured as the whole chain it renders and
/// carried in the variant that names the step — the alternative would be
/// to lose everything the encoder said about why the module was refused.
fn encode_component(path: &Path) -> Result<Vec<u8>, WasmBuildError> {
    let wasm = fs::read(path).map_err(|source| WasmBuildError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if WasmParser::is_component(&wasm) {
        return Ok(wasm);
    }
    ComponentEncoder::default()
        .module(&wasm)
        .map_err(|error| WasmBuildError::LoadCoreModule {
            path: path.display().to_string(),
            report: format!("{error:#}"),
        })?
        .validate(true)
        .encode()
        .map_err(|error| WasmBuildError::EncodeComponent {
            path: path.display().to_string(),
            report: format!("{error:#}"),
        })
}

fn ensure_root_keypair(
    root_secret_path: &Path,
    root_public_path: &Path,
) -> Result<SigningKey, KeyError> {
    let signing_key = if root_secret_path.is_file() {
        read_signing_key(root_secret_path)?
    } else {
        let mut secret = SecretKey::default();
        SysRng
            .try_fill_bytes(&mut secret)
            .map_err(|source| KeyError::Entropy { source })?;
        let signing_key = SigningKey::from_bytes(&secret);
        fs::write(root_secret_path, signing_key.to_bytes()).map_err(|source| KeyError::Write {
            path: root_secret_path.display().to_string(),
            source,
        })?;
        signing_key
    };
    // The public key is rewritten either way: it is the copy the kernel
    // verifies against, and a secret that outlived its public half would
    // otherwise sign artifacts nothing can check.
    fs::write(
        root_public_path,
        VerifyingKey::from(&signing_key).to_bytes(),
    )
    .map_err(|source| KeyError::Write {
        path: root_public_path.display().to_string(),
        source,
    })?;
    Ok(signing_key)
}

fn read_signing_key(path: &Path) -> Result<SigningKey, KeyError> {
    let bytes = fs::read(path).map_err(|source| KeyError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let secret_bytes: [u8; 32] =
        bytes
            .as_slice()
            .try_into()
            .map_err(|source| KeyError::NotThirtyTwoBytes {
                path: path.display().to_string(),
                source,
            })?;
    Ok(SigningKey::from_bytes(&secret_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_target_mismatches_fail_fast() {
        let mut artifact = test_artifact(
            "simd-lanes",
            "helios-wasix-conformance",
            "0.1.0",
            "tools/wasi-apps/wasix-tests",
            Path::new("simd-lanes.wasm"),
        );
        artifact.targets.push("aarch64-unknown-none".to_owned());
        let selected = Some(BTreeSet::from(["simd-lanes".to_owned()]));

        let error =
            reject_selected_target_mismatches(&[artifact], "riscv64gc-unknown-none-elf", &selected)
                .expect_err("target-specific artifact must reject unsupported selected target");
        assert!(
            error.to_string().contains("simd-lanes supports"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn wasm_simd_requirement_gates_targets() {
        let mut artifact = test_artifact(
            "quickjs",
            "quickjs-ng/quickjs",
            "v0.14.0",
            "https://github.com/quickjs-ng/quickjs",
            Path::new("qjs.wasm"),
        );
        artifact.requires_wasm_simd = true;

        assert!(artifact.supports_target("aarch64-unknown-none"));
        assert!(artifact.supports_target("x86_64-unknown-none"));
        assert!(artifact.supports_target("aarch64-apple-darwin"));
        assert!(!artifact.supports_target("riscv64gc-unknown-none-elf"));

        let selected = Some(BTreeSet::from(["quickjs".to_owned()]));
        let error =
            reject_selected_target_mismatches(&[artifact], "riscv64gc-unknown-none-elf", &selected)
                .expect_err("simd-requiring artifact must reject riscv64 selection");
        assert!(
            error.to_string().contains("quickjs requires wasm SIMD"),
            "unexpected error: {error}"
        );
    }

    fn test_artifact(
        command: &str,
        package: &str,
        version: &str,
        source_url: &str,
        source: &Path,
    ) -> ExternalBootArtifact {
        ExternalBootArtifact {
            command: command.to_owned(),
            package: package.to_owned(),
            version: version.to_owned(),
            source_url: source_url.to_owned(),
            targets: Vec::new(),
            requires_wasm_simd: false,
            bootfs_path: format!("bin/{command}"),
            source: source.to_owned(),
            support_root: None,
            support_bootfs_prefix: None,
        }
    }
}

/// GitHub's REST API, where a release's tag and its assets are read from.
const GITHUB_API: &str = "https://api.github.com";

/// Environment variables a GitHub token is read from, in order: the first
/// is what a GitHub Actions runner sets, the second what `gh auth` sets.
///
/// The API answers a public repository without one, at sixty requests an
/// hour per address, which a busy shared runner can be a long way into.
const GITHUB_TOKEN_VARIABLES: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];

/// Digits of the HTTP status curl is asked to write after the body.
const HTTP_STATUS_DIGITS: usize = 3;

/// Bytes of an unexpected answer an error carries: enough to read
/// GitHub's own `{"message": ...}`, not enough to bury the message.
const ERROR_BODY_BYTES: usize = 512;

/// The parts of a GitHub release this fetch reads.
#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

/// One asset of a release.
#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Downloads the kernel profile a release published into the store.
///
/// The store is what `helios-inspector vm --release` reads to build an
/// x86-64 kernel the way the release's own kernel was built
/// (`docs/pgo.md`), so this is the one entry point that puts a profile
/// there: everything else names a profile it was given.
fn run_profile_fetch(
    command: ProfileFetchCommand,
    explicit_workspace_root: Option<&Path>,
) -> Result<(), ProfileFetchError> {
    let workspace_root = WorkspaceRoot::resolve(explicit_workspace_root)?;
    let store = KernelProfileStore::new(workspace_root.path());
    let repository = &command.repository;
    let url = match &command.tag {
        Some(tag) => format!("{GITHUB_API}/repos/{repository}/releases/tags/{tag}"),
        None => format!("{GITHUB_API}/repos/{repository}/releases/latest"),
    };
    let document = github_get(&url, true)?;
    let release: GithubRelease =
        serde_json::from_slice(&document).map_err(|source| ProfileFetchError::DecodeRelease {
            url: url.clone(),
            source,
        })?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == KERNEL_PROFILE_ASSET)
        .ok_or_else(|| ProfileFetchError::NoProfileAsset {
            repository: repository.clone(),
            tag: release.tag_name.clone(),
        })?;
    let path = store.profile_path(&release.tag_name)?;
    let directory = path.parent().expect("a profile path names its release");
    fs::create_dir_all(directory).map_err(|source| ProfileFetchError::CreateDirectory {
        path: directory.display().to_string(),
        source,
    })?;
    // The download carries no token: GitHub redirects an asset to its own
    // object store, and an Authorization header follows the redirect
    // there.
    let profile = github_get(&asset.browser_download_url, false)?;
    fs::write(&path, profile).map_err(|source| ProfileFetchError::Write {
        path: path.display().to_string(),
        source,
    })?;
    // The header check before the record: a record names a profile a
    // build can read, or there is no record.
    helios_profdata::validate(&path)?;
    store.publish(&FetchedProfile {
        repository: repository.clone(),
        tag: release.tag_name.clone(),
    })?;
    println!("{} {} {}", release.tag_name, repository, path.display());
    Ok(())
}

/// One HTTPS GET through curl, returning the body.
///
/// curl is the HTTP client this repository already downloads its pinned
/// artifacts with (`tools/wasi-apps/build.sh`). The status is asked for
/// explicitly rather than through `--fail`, because a 404 on a release is
/// the answer that has something to say and `--fail` throws the body away.
fn github_get(url: &str, authenticated: bool) -> Result<Vec<u8>, ProfileFetchError> {
    let config = if authenticated {
        github_token_header()?
    } else {
        String::new()
    };
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--write-out",
            "%{http_code}",
            "--config",
            "-",
        ])
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ProfileFetchError::Curl {
            url: url.to_owned(),
            source,
        })?;
    child
        .stdin
        .take()
        .expect("curl was spawned with a piped stdin")
        .write_all(config.as_bytes())
        .map_err(|source| ProfileFetchError::CurlConfig {
            url: url.to_owned(),
            source,
        })?;
    let output = child
        .wait_with_output()
        .map_err(|source| ProfileFetchError::Curl {
            url: url.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(ProfileFetchError::CurlExited {
            url: url.to_owned(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let mut body = output.stdout;
    if body.len() < HTTP_STATUS_DIGITS {
        return Err(ProfileFetchError::NoHttpStatus {
            url: url.to_owned(),
            len: body.len(),
        });
    }
    let status = String::from_utf8_lossy(&body[body.len() - HTTP_STATUS_DIGITS..]).into_owned();
    body.truncate(body.len() - HTTP_STATUS_DIGITS);
    match status.as_str() {
        "200" => Ok(body),
        "404" => Err(ProfileFetchError::NoRelease {
            url: url.to_owned(),
        }),
        _ => Err(ProfileFetchError::HttpStatus {
            url: url.to_owned(),
            status,
            body: String::from_utf8_lossy(&body[..body.len().min(ERROR_BODY_BYTES)])
                .trim()
                .to_owned(),
        }),
    }
}

/// The curl configuration carrying the GitHub token, or nothing.
///
/// The token reaches curl on its standard input rather than in an
/// argument, because arguments are readable to every process on the
/// machine. A value that is not a token is refused rather than quoted
/// into a configuration line.
fn github_token_header() -> Result<String, ProfileFetchError> {
    for variable in GITHUB_TOKEN_VARIABLES {
        let Ok(token) = std::env::var(variable) else {
            continue;
        };
        if token.is_empty() {
            continue;
        }
        if !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(ProfileFetchError::Token { variable });
        }
        return Ok(format!("header = \"Authorization: Bearer {token}\"\n"));
    }
    Ok(String::new())
}
