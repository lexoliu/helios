//! Run-time discovery of the Cargo workspace root a host tool is operating on.
//!
//! `helios-cli` and `helios-inspector` resolve repository-relative paths — the
//! kernel-prebuild manifest, program manifests, bootfs sources, build
//! artifacts — against a checkout of this repository. That checkout is the one
//! the tool is *run* in, never the one it happened to be compiled in: a binary
//! built in one worktree and reused from another (which is what a git worktree
//! with a warm `target/` is) would otherwise read and write the wrong tree
//! until it is rebuilt.
//!
//! Resolution order is an explicit path from the caller, then
//! [`WORKSPACE_ROOT_ENV`], then the nearest ancestor of the current directory
//! whose `Cargo.toml` declares a `[workspace]` table. Every failure is typed
//! and names the directory it was looking at.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

use thiserror::Error;

/// Environment variable naming the workspace root explicitly.
///
/// It exists for callers that run a Helios host tool from outside a checkout;
/// inside a checkout the directory walk is authoritative.
pub const WORKSPACE_ROOT_ENV: &str = "HELIOS_WORKSPACE_ROOT";

/// The manifest file that marks a directory as a Cargo workspace root.
const WORKSPACE_MANIFEST_FILE: &str = "Cargo.toml";

/// The table a workspace root's manifest must declare.
const WORKSPACE_TABLE: &str = "workspace";

/// Every way workspace-root resolution can fail.
#[derive(Debug, Error)]
pub enum WorkspaceRootError {
    /// The process has no readable current directory to walk up from.
    #[error("failed to read the current directory")]
    CurrentDir(#[source] io::Error),
    /// No ancestor of the starting directory is a Cargo workspace root.
    #[error(
        "no Cargo workspace root at or above {start}: \
         run inside a Helios checkout, pass an explicit workspace root, \
         or set {WORKSPACE_ROOT_ENV}",
        start = .start.display()
    )]
    NotFound {
        /// The directory the upward walk started from.
        start: PathBuf,
    },
    /// A candidate directory could not be resolved to a real path.
    #[error("failed to resolve {path}", path = .path.display())]
    Resolve {
        /// The path that could not be resolved.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A `Cargo.toml` on the way up exists but could not be read.
    #[error("failed to read {manifest}", manifest = .manifest.display())]
    ReadManifest {
        /// The manifest that could not be read.
        manifest: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A `Cargo.toml` on the way up is not valid TOML.
    #[error("failed to parse {manifest}", manifest = .manifest.display())]
    ParseManifest {
        /// The manifest that could not be parsed.
        manifest: PathBuf,
        /// The underlying TOML error.
        #[source]
        source: toml::de::Error,
    },
    /// An explicitly named directory is not a Cargo workspace root.
    #[error(
        "{root} is not a Cargo workspace root: {manifest} declares no [{WORKSPACE_TABLE}] table",
        root = .root.display(),
        manifest = .manifest.display()
    )]
    NotAWorkspace {
        /// The directory that was named explicitly.
        root: PathBuf,
        /// The manifest that was inspected.
        manifest: PathBuf,
    },
    /// An explicitly named directory holds no manifest at all.
    #[error(
        "{root} is not a Cargo workspace root: {manifest} does not exist",
        root = .root.display(),
        manifest = .manifest.display()
    )]
    MissingManifest {
        /// The directory that was named explicitly.
        root: PathBuf,
        /// The manifest that was expected there.
        manifest: PathBuf,
    },
}

/// An absolute path to a directory whose `Cargo.toml` declares `[workspace]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Resolves the workspace root a host tool should operate on.
    ///
    /// `explicit` is a path the caller was given on its command line and wins
    /// over everything else; [`WORKSPACE_ROOT_ENV`] comes next; otherwise the
    /// current directory's nearest workspace ancestor is used.
    pub fn resolve(explicit: Option<&Path>) -> Result<Self, WorkspaceRootError> {
        if let Some(path) = explicit {
            return Self::from_explicit(path);
        }
        if let Some(path) = env::var_os(WORKSPACE_ROOT_ENV) {
            return Self::from_explicit(Path::new(&path));
        }
        let current = env::current_dir().map_err(WorkspaceRootError::CurrentDir)?;
        Self::discover_from(&current)
    }

    /// Accepts a directory the caller named, verifying it really is a root.
    pub fn from_explicit(root: &Path) -> Result<Self, WorkspaceRootError> {
        let root = canonicalize(root)?;
        let manifest = root.join(WORKSPACE_MANIFEST_FILE);
        if !manifest.is_file() {
            return Err(WorkspaceRootError::MissingManifest { root, manifest });
        }
        if declares_workspace(&manifest)? {
            Ok(Self(root))
        } else {
            Err(WorkspaceRootError::NotAWorkspace { root, manifest })
        }
    }

    /// Walks up from `start` to the nearest directory that is a workspace root.
    pub fn discover_from(start: &Path) -> Result<Self, WorkspaceRootError> {
        let start = canonicalize(start)?;
        for candidate in start.ancestors() {
            let manifest = candidate.join(WORKSPACE_MANIFEST_FILE);
            if manifest.is_file() && declares_workspace(&manifest)? {
                return Ok(Self(candidate.to_path_buf()));
            }
        }
        Err(WorkspaceRootError::NotFound { start })
    }

    /// The absolute path of the workspace root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Resolves a workspace-relative path, leaving an absolute path alone.
    #[must_use]
    pub fn join(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.0.join(path)
        }
    }
}

impl AsRef<Path> for WorkspaceRoot {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, WorkspaceRootError> {
    fs::canonicalize(path).map_err(|source| WorkspaceRootError::Resolve {
        path: path.to_path_buf(),
        source,
    })
}

fn declares_workspace(manifest: &Path) -> Result<bool, WorkspaceRootError> {
    let text = fs::read_to_string(manifest).map_err(|source| WorkspaceRootError::ReadManifest {
        manifest: manifest.to_path_buf(),
        source,
    })?;
    let table =
        text.parse::<toml::Table>()
            .map_err(|source| WorkspaceRootError::ParseManifest {
                manifest: manifest.to_path_buf(),
                source,
            })?;
    Ok(table.contains_key(WORKSPACE_TABLE))
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceRoot, WorkspaceRootError};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn workspace_tree() -> TempDir {
        let temp = TempDir::new().expect("temporary directory must be creatable");
        fs::write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"cli\"]\nresolver = \"2\"\n",
        )
        .expect("workspace manifest must be writable");
        let member = temp.path().join("cli/src");
        fs::create_dir_all(&member).expect("member directory must be creatable");
        fs::write(
            temp.path().join("cli/Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .expect("member manifest must be writable");
        temp
    }

    fn canonical(path: &Path) -> std::path::PathBuf {
        fs::canonicalize(path).expect("path must resolve")
    }

    #[test]
    fn discovers_the_root_from_a_nested_directory() {
        let tree = workspace_tree();
        let nested = tree.path().join("cli/src");
        let root = WorkspaceRoot::discover_from(&nested).expect("nested directory must resolve");
        assert_eq!(root.path(), canonical(tree.path()));
    }

    #[test]
    fn discovers_the_root_from_the_root_itself() {
        let tree = workspace_tree();
        let root = WorkspaceRoot::discover_from(tree.path()).expect("root must resolve");
        assert_eq!(root.path(), canonical(tree.path()));
    }

    #[test]
    fn skips_a_member_manifest_that_declares_no_workspace() {
        let tree = workspace_tree();
        let member = tree.path().join("cli");
        let root = WorkspaceRoot::discover_from(&member).expect("member directory must resolve");
        assert_eq!(root.path(), canonical(tree.path()));
    }

    #[test]
    fn fails_outside_any_workspace() {
        let outside = TempDir::new().expect("temporary directory must be creatable");
        let nested = outside.path().join("deep/nested");
        fs::create_dir_all(&nested).expect("nested directory must be creatable");
        let error = WorkspaceRoot::discover_from(&nested)
            .expect_err("a directory outside a workspace must not resolve");
        let WorkspaceRootError::NotFound { start } = error else {
            panic!("expected a NotFound error, got {error:?}");
        };
        assert_eq!(start, canonical(&nested));
    }

    #[test]
    fn rejects_an_explicit_root_that_is_not_a_workspace() {
        let tree = workspace_tree();
        let member = tree.path().join("cli");
        let error = WorkspaceRoot::from_explicit(&member)
            .expect_err("a member directory is not a workspace root");
        assert!(
            matches!(error, WorkspaceRootError::NotAWorkspace { .. }),
            "expected a NotAWorkspace error, got {error:?}"
        );
    }

    #[test]
    fn rejects_an_explicit_root_without_a_manifest() {
        let outside = TempDir::new().expect("temporary directory must be creatable");
        let error = WorkspaceRoot::from_explicit(outside.path())
            .expect_err("a directory without a manifest is not a workspace root");
        assert!(
            matches!(error, WorkspaceRootError::MissingManifest { .. }),
            "expected a MissingManifest error, got {error:?}"
        );
    }

    #[test]
    fn joins_relative_paths_and_passes_absolute_paths_through() {
        let tree = workspace_tree();
        let root = WorkspaceRoot::discover_from(tree.path()).expect("root must resolve");
        assert_eq!(
            root.join(Path::new("programs/init/Cargo.toml")),
            canonical(tree.path()).join("programs/init/Cargo.toml")
        );
        let absolute = Path::new("/absolute/bootfs");
        assert_eq!(root.join(absolute), absolute);
    }
}
