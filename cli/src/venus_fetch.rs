//! `helios-cli venus-protocol-fetch`: fills the cache directory the
//! `helios-venus` build script reads, from the URLs and hashes of
//! `venus/protocol-inputs.toml`.
//!
//! Every file is checked against its pinned SHA-256 before it replaces
//! anything, so the cache holds a file that matches the manifest or no file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use helios_venus_gen::inputs::{
    CACHE_DIR_ENV, Inputs, InputsError, MANIFEST_PATH, Manifest, PinnedFile, sha256_hex,
    verify_file,
};

#[derive(Debug, thiserror::Error)]
pub enum VenusFetchError {
    #[error("{0}")]
    Inputs(#[from] InputsError),
    #[error("failed to run curl for {url}: {source}; the fetch downloads over HTTPS with curl")]
    Curl {
        url: String,
        #[source]
        source: io::Error,
    },
    #[error("curl did not fetch {url}: it exited {status} and said {stderr}")]
    CurlExited {
        url: String,
        status: String,
        stderr: String,
    },
    #[error(
        "{url} answered a file with SHA-256 {actual}, but the manifest pins {expected} for \
         {path}; nothing was stored"
    )]
    HashMismatch {
        url: String,
        path: String,
        expected: String,
        actual: String,
    },
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Fetches every file of the manifest that the cache directory does not hold
/// at its pinned hash.
pub fn run(workspace_root: &Path) -> Result<(), VenusFetchError> {
    let manifest = Manifest::read(&workspace_root.join(MANIFEST_PATH))?;
    let cache = Inputs::cache_dir(workspace_root, std::env::var(CACHE_DIR_ENV).ok().as_deref());
    for file in &manifest.files {
        match verify_file(&cache, file) {
            Ok(()) => continue,
            // Absent or stale: fetch it again.
            Err(InputsError::Missing { .. } | InputsError::HashMismatch { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        fetch_file(&cache, file)?;
    }
    // Everything is in place and verified, whichever way it got there.
    Inputs::open(manifest, &cache)?;
    Ok(())
}

fn fetch_file(cache: &Path, file: &PinnedFile) -> Result<(), VenusFetchError> {
    let destination = cache.join(&file.path);
    let directory = destination
        .parent()
        .expect("a manifest path names a file inside the cache");
    fs::create_dir_all(directory).map_err(|source| VenusFetchError::Io {
        action: "create",
        path: directory.to_owned(),
        source,
    })?;
    // `--fail` turns an HTTP error into a nonzero exit; `--retry` covers the
    // transient failures a shared runner sees.
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
        ])
        .arg(&file.url)
        .output()
        .map_err(|source| VenusFetchError::Curl {
            url: file.url.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(VenusFetchError::CurlExited {
            url: file.url.clone(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let actual = sha256_hex(&output.stdout);
    if actual != file.sha256 {
        return Err(VenusFetchError::HashMismatch {
            url: file.url.clone(),
            path: file.path.clone(),
            expected: file.sha256.clone(),
            actual,
        });
    }
    // Written beside the destination and renamed, so a reader never sees half
    // a file.
    let partial = destination.with_extension("part");
    fs::write(&partial, &output.stdout).map_err(|source| VenusFetchError::Io {
        action: "write",
        path: partial.clone(),
        source,
    })?;
    fs::rename(&partial, &destination).map_err(|source| VenusFetchError::Io {
        action: "rename into place",
        path: destination,
        source,
    })
}
