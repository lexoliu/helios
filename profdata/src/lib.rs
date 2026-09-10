//! The kernel profile a `-C profile-use` build reads, and the store the
//! fetched one lives in.
//!
//! Two host tools hold one end of this each. `helios-cli profile-fetch`
//! downloads the `helios-kernel.profdata` a `kernel-profile.yml` run
//! uploaded, or a release attached, and writes it into the store;
//! `helios-inspector vm --release` reads the store to build the x86-64
//! kernel against it (`docs/pgo.md`, #226, #313). What a profile has to
//! be, and where a fetched one lives, is therefore one definition rather
//! than a convention two crates keep separately.
//!
//! `-C profile-use` takes the *indexed* profile `llvm-profdata merge`
//! writes, not the `.profraw` the guest kernel exports
//! (`kernel/src/profiling`). The two containers are distinguished by
//! their first eight bytes and the second word of an indexed file is its
//! format version, so a profile from another toolchain is recognisable
//! before rustc is started — and is refused here rather than deep inside
//! a twenty-minute kernel build whose failure names an LLVM bitcode
//! error.
//!
//! The check mirrors the guest writer's: it holds one pinned version
//! word, says which toolchain that word was read from, and fails loudly
//! when a bump moves the format.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

mod store;

pub use store::{FetchedProfile, KernelProfileStore, KernelProfileStoreError};

/// Name of the release asset every release carries the kernel's profile
/// under (#226), of the file inside the collection artifact, and of the
/// file the store keeps it in.
///
/// `release.yml`'s `kernel-profile` job attaches it and
/// `kernel-profile.yml` uploads it; nothing else names it, so a rename
/// is one edit.
pub const KERNEL_PROFILE_ASSET: &str = "helios-kernel.profdata";

/// Name of the workflow artifact a `kernel-profile.yml` run uploads the
/// profile as (#313), which is what a fetch without `--tag` resolves.
pub const KERNEL_PROFILE_ARTIFACT: &str = "helios-kernel-profdata";

/// The workflow that collects the profile on demand, named by every
/// refusal whose answer is to dispatch it.
pub const KERNEL_PROFILE_WORKFLOW: &str = "kernel-profile.yml";

/// The command that puts a profile in the store, spelled the way a user
/// would type it.
///
/// Every refusal that comes of an empty store names it, so the fix is in
/// the error rather than in the documentation.
pub const FETCH_COMMAND: &str = "helios-cli profile-fetch";

/// The repository whose collections and releases carry the kernel
/// profile.
///
/// This is where Helios publishes, the way `checkout-wasmtime` pins where
/// the vendored Wasmtime comes from. `helios-cli profile-fetch --repo`
/// overrides it for a fork.
pub const RELEASE_REPOSITORY: &str = "lexoliu/helios";

/// `IndexedInstrProf::Magic`, the first eight bytes of a merged profile
/// read as a little-endian word: the ASCII `\xfflprofi\x81`.
const INDEXED_MAGIC: u64 = 0x8169_666f_7270_6cff;

/// `INSTR_PROF_RAW_MAGIC_64` as the guest writes it (`kernel/src/profiling/raw.rs`),
/// read the same way. A raw profile reaching a `--profile-use` build means
/// the `llvm-profdata merge` step was skipped, which is worth its own
/// sentence rather than "not an indexed profile".
const RAW_MAGIC: u64 = 0xff6c_7072_6f66_7281;

/// `VARIANT_MASKS_ALL`: the high byte of the version word carries the
/// format's variant bits, and the version itself is what is left.
const VARIANT_MASKS_ALL: u64 = 0xff00_0000_0000_0000;

/// `VARIANT_MASK_IR_PROF`: the profile came from IR-level instrumentation,
/// which is what `-C profile-generate` asks rustc for and therefore what
/// every profile the guest kernel produces carries.
const VARIANT_MASK_IR_PROF: u64 = 1 << 56;

/// Indexed-profile format version `-C profile-use` reads on the pinned
/// toolchain.
///
/// Read from the toolchain rather than guessed at: on the pinned nightly
/// (`rust-toolchain.toml`, `nightly-2026-06-15`, `rustc -vV` reports LLVM
/// 22.1.6) `llvm-profdata merge` writes the word `0x0100_0000_0000_000d`
/// — version 13, with the IR-instrumentation variant bit. Thirteen is
/// `IndexedInstrProf::ProfVersion::CurrentVersion` in LLVM's
/// `llvm/include/llvm/ProfileData/InstrProf.h`. A toolchain bump that
/// moves the format therefore refuses the old artifact by name instead of
/// handing rustc a profile it reads as garbage.
const INDEXED_PROFILE_VERSION: u64 = 13;

/// Bytes of an indexed profile the header check needs: the magic and the
/// version word.
const HEADER_BYTES: usize = 16;

/// Why a file named by `--profile-use` is not a profile this toolchain
/// can build against.
#[derive(Debug, thiserror::Error)]
pub enum ProfileUseError {
    /// The file could not be opened or read.
    #[error("failed to read the profile {path}: {source}")]
    Read {
        /// The profile that could not be read.
        path: String,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The file is shorter than the header the check reads.
    #[error(
        "the profile {path} is {len} bytes; an indexed profile starts with a \
         {HEADER_BYTES}-byte magic and version"
    )]
    TooShort {
        /// The profile that is too short.
        path: String,
        /// How many bytes it holds.
        len: usize,
    },
    /// The file is the raw profile the guest exports, not the merged one.
    #[error(
        "{path} is a raw profile (.profraw), and -C profile-use reads the merged one; \
         `llvm-profdata merge --output <file>.profdata {path}` produces it"
    )]
    NotMerged {
        /// The raw profile that reached a `--profile-use` build.
        path: String,
    },
    /// The file is not an LLVM profile at all.
    #[error(
        "{path} is not an LLVM profile: it starts with {magic:#018x}, and an indexed \
         profile starts with {INDEXED_MAGIC:#018x}"
    )]
    NotAProfile {
        /// The file that is not a profile.
        path: String,
        /// The magic word it does start with.
        magic: u64,
    },
    /// The profile's format version is not the one this toolchain reads.
    #[error(
        "{path} is an indexed profile of version {found}, and this toolchain reads \
         version {expected} (docs/pgo.md); collect the profile again on the toolchain \
         rust-toolchain.toml pins"
    )]
    VersionMismatch {
        /// The profile whose version does not match.
        path: String,
        /// The version the file carries.
        found: u64,
        /// The version this toolchain reads.
        expected: u64,
    },
    /// The profile did not come from IR-level instrumentation.
    #[error(
        "{path} carries version word {version:#018x}, which does not set the \
         IR-instrumentation variant bit {VARIANT_MASK_IR_PROF:#018x}; the kernel is \
         instrumented with -C profile-generate, whose profiles are IR profiles"
    )]
    NotIrInstrumented {
        /// The profile that is not an IR profile.
        path: String,
        /// The version word it carries.
        version: u64,
    },
}

/// Refuses a profile this toolchain's `-C profile-use` cannot read.
///
/// Called before the build command is assembled, so that a stale artifact
/// costs a header read rather than a kernel compile, and again by
/// `helios-cli profile-fetch` before a downloaded asset enters the store,
/// so that a bad asset is refused where it arrives.
pub fn validate(path: &Path) -> Result<(), ProfileUseError> {
    let name = path.display().to_string();
    let on_read = |source| ProfileUseError::Read {
        path: name.clone(),
        source,
    };
    let mut header = [0u8; HEADER_BYTES];
    let mut file = File::open(path).map_err(on_read)?;
    let mut filled = 0;
    while filled < HEADER_BYTES {
        let read = file.read(&mut header[filled..]).map_err(on_read)?;
        if read == 0 {
            return Err(ProfileUseError::TooShort {
                path: name,
                len: filled,
            });
        }
        filled += read;
    }
    check_header(&name, &header)
}

/// The header check itself, over bytes rather than a file, so every
/// refusal has a test.
fn check_header(path: &str, header: &[u8; HEADER_BYTES]) -> Result<(), ProfileUseError> {
    let word = |offset: usize| {
        u64::from_le_bytes(
            header[offset..offset + 8]
                .try_into()
                .expect("eight bytes of a sixteen-byte header"),
        )
    };
    let magic = word(0);
    if magic == RAW_MAGIC {
        return Err(ProfileUseError::NotMerged {
            path: path.to_owned(),
        });
    }
    if magic != INDEXED_MAGIC {
        return Err(ProfileUseError::NotAProfile {
            path: path.to_owned(),
            magic,
        });
    }
    let version = word(8);
    let found = version & !VARIANT_MASKS_ALL;
    if found != INDEXED_PROFILE_VERSION {
        return Err(ProfileUseError::VersionMismatch {
            path: path.to_owned(),
            found,
            expected: INDEXED_PROFILE_VERSION,
        });
    }
    if version & VARIANT_MASK_IR_PROF == 0 {
        return Err(ProfileUseError::NotIrInstrumented {
            path: path.to_owned(),
            version,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sixteen bytes a merged profile of the pinned toolchain starts
    /// with, for a test that needs a file rather than a header.
    pub(crate) fn pinned_header() -> [u8; HEADER_BYTES] {
        header(
            INDEXED_MAGIC,
            INDEXED_PROFILE_VERSION | VARIANT_MASK_IR_PROF,
        )
    }

    fn header(magic: u64, version: u64) -> [u8; HEADER_BYTES] {
        let mut bytes = [0u8; HEADER_BYTES];
        bytes[..8].copy_from_slice(&magic.to_le_bytes());
        bytes[8..].copy_from_slice(&version.to_le_bytes());
        bytes
    }

    #[test]
    fn the_pinned_toolchains_profile_is_accepted() {
        check_header("pinned.profdata", &pinned_header())
            .expect("the version the pinned toolchain writes is the one this build reads");
    }

    #[test]
    fn a_raw_profile_names_the_merge_step() {
        let error = check_header(
            "boot.profraw",
            &header(RAW_MAGIC, 10 | VARIANT_MASK_IR_PROF),
        )
        .expect_err("-C profile-use reads the merged profile, not the raw one");
        assert!(
            matches!(error, ProfileUseError::NotMerged { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("llvm-profdata merge"));
    }

    #[test]
    fn another_toolchains_profile_names_both_versions() {
        let older = INDEXED_PROFILE_VERSION - 1;
        let error = check_header(
            "old.profdata",
            &header(INDEXED_MAGIC, older | VARIANT_MASK_IR_PROF),
        )
        .expect_err("a profile from another LLVM is refused, not compiled against");
        let ProfileUseError::VersionMismatch {
            found, expected, ..
        } = error
        else {
            panic!("expected a version mismatch, got {error}");
        };
        assert_eq!(found, older);
        assert_eq!(expected, INDEXED_PROFILE_VERSION);
    }

    #[test]
    fn a_front_end_profile_is_refused() {
        let error = check_header(
            "clang.profdata",
            &header(INDEXED_MAGIC, INDEXED_PROFILE_VERSION),
        )
        .expect_err("the kernel is instrumented at IR level and its profile says so");
        assert!(
            matches!(error, ProfileUseError::NotIrInstrumented { .. }),
            "{error}"
        );
    }

    #[test]
    fn something_else_entirely_is_not_a_profile() {
        let error = check_header("kernel.elf", &header(0x0001_0102_464c_457f, 0))
            .expect_err("a file that is not a profile is named as such");
        assert!(
            matches!(error, ProfileUseError::NotAProfile { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_short_file_is_refused_by_length() {
        let directory = tempfile::tempdir().expect("a temporary directory for the short profile");
        let path = directory.path().join("short.profdata");
        std::fs::write(&path, b"\xffl").expect("writing two bytes");
        let error = validate(&path).expect_err("two bytes are not a profile header");
        assert!(
            matches!(error, ProfileUseError::TooShort { len: 2, .. }),
            "{error}"
        );
    }
}
