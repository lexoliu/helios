//! The on-disk form of the build's Ed25519 root secret key.
//!
//! `helios-cli` writes and reads the file when it signs a bootfs, and
//! `helios-inspector` reads it to prove the kernel image it built does not
//! carry it. Both agree on the name, the owner-only mode and the 32-byte
//! seed layout here, so a change to the format is made once.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use thiserror::Error;

/// The root secret key's file name inside a prebuild directory.
pub const ROOT_SECRET_FILE: &str = "helios-root-secret.key";

/// The root public key's file name inside a prebuild directory.
pub const ROOT_PUBLIC_FILE: &str = "helios-root-public.key";

/// The mode a secret key file is created with: its owner reads and writes
/// it, and nobody else can open it.
pub const SECRET_KEY_MODE: u32 = 0o600;

/// The permission bits of everyone but the owner.
const GROUP_AND_OTHER: u32 = 0o077;

/// Why a root secret key file could not be read.
#[derive(Debug, Error)]
pub enum RootSecretReadError {
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
    #[error(
        "{path} is a secret key with mode {mode:03o}; it must be readable by its owner alone (600)"
    )]
    SecretKeyExposed { path: String, mode: u32 },
}

/// Reads the 32-byte Ed25519 seed a root secret key file holds, refusing a
/// file anyone but its owner can open.
pub fn read_root_secret_seed(path: &Path) -> Result<[u8; 32], RootSecretReadError> {
    let read_error = |source| RootSecretReadError::Read {
        path: path.display().to_string(),
        source,
    };
    let mut file = fs::File::open(path).map_err(read_error)?;
    // Judged on the open handle, so the file read is the file checked.
    let mode = file.metadata().map_err(read_error)?.permissions().mode() & 0o777;
    if mode & GROUP_AND_OTHER != 0 {
        return Err(RootSecretReadError::SecretKeyExposed {
            path: path.display().to_string(),
            mode,
        });
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(read_error)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|source| RootSecretReadError::NotThirtyTwoBytes {
            path: path.display().to_string(),
            source,
        })
}
