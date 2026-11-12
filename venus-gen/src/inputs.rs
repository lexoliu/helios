//! The pinned venus-protocol inputs: the manifest that names them, the cache
//! directory they are fetched into, and the verification every reader goes
//! through.
//!
//! Nothing here touches the network. `helios-cli venus-protocol-fetch` fills
//! the cache; the build script and the tests only read it, through
//! [`Inputs::open`], which refuses a missing file or a hash that differs from
//! the manifest.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The command that fills the cache directory.
pub const FETCH_COMMAND: &str = "cargo run -p helios-cli -- venus-protocol-fetch";

/// The environment variable that moves the cache directory.
pub const CACHE_DIR_ENV: &str = "HELIOS_VENUS_PROTOCOL_DIR";

/// The manifest's path relative to the workspace root.
pub const MANIFEST_PATH: &str = "venus/protocol-inputs.toml";

/// Where the fetched files live, relative to the workspace root, when
/// [`CACHE_DIR_ENV`] is not set.
pub const DEFAULT_CACHE_DIR: &str = "target/venus-protocol";

/// Why the inputs cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum InputsError {
    #[error("failed to read the venus-protocol manifest {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse the venus-protocol manifest {path}: {source}")]
    ParseManifest {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("manifest entry {path} is not a plain relative path")]
    BadEntryPath { path: String },
    #[error("manifest entry {path} names {url}, which is not under commit {commit}")]
    UrlNotAtCommit {
        path: String,
        url: String,
        commit: String,
    },
    #[error("manifest entry {path} has a SHA-256 that is not 64 lowercase hex digits: {sha256}")]
    BadHash { path: String, sha256: String },
    #[error("manifest lists {path} more than once")]
    DuplicateEntry { path: String },
    #[error("pinned input {path} is missing; fetch it with `{FETCH_COMMAND}`")]
    Missing { path: PathBuf },
    #[error("failed to read pinned input {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "pinned input {path} does not match the manifest: expected SHA-256 {expected}, found \
         {actual}; refetch it with `{FETCH_COMMAND}`"
    )]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

/// One pinned file.
#[derive(Clone, Debug, Deserialize)]
pub struct PinnedFile {
    /// Path relative to the cache directory (and to the upstream repository).
    pub path: String,
    /// Raw-file URL at the pinned commit.
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
}

/// The committed manifest.
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub repository: String,
    pub tag: String,
    /// The full commit SHA every URL is taken at.
    pub commit: String,
    pub files: Vec<PinnedFile>,
}

impl Manifest {
    /// Reads and validates the manifest at `path`.
    pub fn read(path: &Path) -> Result<Self, InputsError> {
        let text = fs::read_to_string(path).map_err(|source| InputsError::ReadManifest {
            path: path.to_owned(),
            source,
        })?;
        let manifest: Self =
            toml::from_str(&text).map_err(|source| InputsError::ParseManifest {
                path: path.to_owned(),
                source,
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), InputsError> {
        let mut seen = std::collections::BTreeSet::new();
        for file in &self.files {
            let plain = Path::new(&file.path)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)));
            if file.path.is_empty() || !plain {
                return Err(InputsError::BadEntryPath {
                    path: file.path.clone(),
                });
            }
            if !file.url.contains(&format!("/{}/", self.commit)) {
                return Err(InputsError::UrlNotAtCommit {
                    path: file.path.clone(),
                    url: file.url.clone(),
                    commit: self.commit.clone(),
                });
            }
            let hex = file.sha256.len() == 64
                && file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if !hex {
                return Err(InputsError::BadHash {
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                });
            }
            if !seen.insert(file.path.as_str()) {
                return Err(InputsError::DuplicateEntry {
                    path: file.path.clone(),
                });
            }
        }
        Ok(())
    }
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The verified cache directory.
#[derive(Clone, Debug)]
pub struct Inputs {
    root: PathBuf,
    manifest: Manifest,
}

impl Inputs {
    /// The cache directory for a workspace rooted at `workspace_root`:
    /// [`CACHE_DIR_ENV`] when set and non-empty, else [`DEFAULT_CACHE_DIR`].
    pub fn cache_dir(workspace_root: &Path, env_override: Option<&str>) -> PathBuf {
        match env_override {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => workspace_root.join(DEFAULT_CACHE_DIR),
        }
    }

    /// Verifies every manifest file in `cache_dir`: present, and equal to the
    /// pinned hash.
    pub fn open(manifest: Manifest, cache_dir: &Path) -> Result<Self, InputsError> {
        for file in &manifest.files {
            verify_file(cache_dir, file)?;
        }
        Ok(Self {
            root: cache_dir.to_owned(),
            manifest,
        })
    }

    /// The manifest the directory was verified against.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The directory the files are in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The absolute path of a pinned file.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// The text of a pinned file.
    pub fn read_to_string(&self, relative: &str) -> Result<String, InputsError> {
        let path = self.path(relative);
        fs::read_to_string(&path).map_err(|source| InputsError::Read { path, source })
    }
}

/// Checks one file against its manifest entry.
pub fn verify_file(cache_dir: &Path, file: &PinnedFile) -> Result<(), InputsError> {
    let path = cache_dir.join(&file.path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(InputsError::Missing { path });
        }
        Err(source) => return Err(InputsError::Read { path, source }),
    };
    let actual = sha256_hex(&bytes);
    if actual != file.sha256 {
        return Err(InputsError::HashMismatch {
            path,
            expected: file.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(sha256: &str) -> Manifest {
        Manifest {
            repository: "https://example.invalid/repo".into(),
            tag: "v1".into(),
            commit: "abc".into(),
            files: vec![PinnedFile {
                path: "dir/file.txt".into(),
                url: "https://example.invalid/repo/-/raw/abc/dir/file.txt".into(),
                sha256: sha256.into(),
            }],
        }
    }

    #[test]
    fn missing_file_names_the_path_and_the_fetch_command() {
        let dir = tempfile::tempdir().unwrap();
        let error = Inputs::open(manifest(&sha256_hex(b"x")), dir.path()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("dir/file.txt"), "{text}");
        assert!(text.contains(FETCH_COMMAND), "{text}");
    }

    #[test]
    fn hash_mismatch_names_both_hashes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("dir")).unwrap();
        fs::write(dir.path().join("dir/file.txt"), b"actual").unwrap();
        let expected = sha256_hex(b"expected");
        let error = Inputs::open(manifest(&expected), dir.path()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(&expected), "{text}");
        assert!(text.contains(&sha256_hex(b"actual")), "{text}");
        assert!(text.contains("dir/file.txt"), "{text}");
    }

    #[test]
    fn matching_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("dir")).unwrap();
        fs::write(dir.path().join("dir/file.txt"), b"same").unwrap();
        let inputs = Inputs::open(manifest(&sha256_hex(b"same")), dir.path()).unwrap();
        assert_eq!(inputs.read_to_string("dir/file.txt").unwrap(), "same");
    }

    #[test]
    fn url_outside_the_commit_is_refused() {
        let mut bad = manifest(&sha256_hex(b"x"));
        bad.files[0].url = "https://example.invalid/repo/-/raw/main/dir/file.txt".into();
        assert!(matches!(
            bad.validate(),
            Err(InputsError::UrlNotAtCommit { .. })
        ));
    }
}
