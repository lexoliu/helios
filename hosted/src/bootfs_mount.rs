use std::fs;
use std::path::{Path, PathBuf};

use helios_hal::fs::{DirectoryRights, FileRights};
use helios_kernel::{BootDirectoryHandleExt, EmbeddedBootFs};
use tempfile::TempDir;
use thiserror::Error;

/// Hosted adapter that exposes the immutable embedded bootfs as a real
/// directory tree for the standard Wasmtime WASI host implementation.
///
/// The kernel keeps bootfs architecture-neutral and capability-based. Hosted
/// mode materializes it into a temporary directory only at the final boundary
/// where the upstream WASI embedding expects an OS-backed filesystem root.
pub struct MountedBootFs {
    root: TempDir,
}

#[derive(Debug, Error)]
pub enum MountError {
    #[error("failed to create hosted bootfs root: {0}")]
    CreateRoot(std::io::Error),
    #[error("failed to create directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write file {path}: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to enumerate host directory {path}: {source}")]
    ReadDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read metadata for {path}: {source}")]
    ReadMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("bootfs merge collision at {path}")]
    Collision { path: PathBuf },
}

impl MountedBootFs {
    pub fn mount(bootfs: EmbeddedBootFs, overlay_root: Option<&Path>) -> Result<Self, MountError> {
        let root = tempfile::Builder::new()
            .prefix("helios-init-root-")
            .tempdir()
            .map_err(MountError::CreateRoot)?;

        materialize_embedded_bootfs(bootfs, root.path())?;

        if let Some(overlay_root) = overlay_root {
            merge_host_tree(overlay_root, root.path())?;
        }

        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }
}

fn materialize_embedded_bootfs(
    bootfs: EmbeddedBootFs,
    destination_root: &Path,
) -> Result<(), MountError> {
    let root = bootfs.root_directory(DirectoryRights::READ);
    copy_embedded_directory(&root, destination_root)
}

fn copy_embedded_directory(
    directory: &helios_hal::fs::DirectoryHandle<helios_kernel::BootDirectory>,
    destination_root: &Path,
) -> Result<(), MountError> {
    fs::create_dir_all(destination_root).map_err(|source| MountError::CreateDirectory {
        path: destination_root.to_path_buf(),
        source,
    })?;

    for entry in directory.list() {
        let destination = destination_root.join(entry.name());

        if entry.is_directory() {
            let child = directory
                .open_directory(entry.name(), DirectoryRights::READ)
                .unwrap_or_else(|| {
                    panic!(
                        "embedded bootfs directory {} disappeared during traversal",
                        entry.name()
                    )
                });
            copy_embedded_directory(&child, &destination)?;
            continue;
        }

        let file = directory
            .open_file(entry.name(), FileRights::READ)
            .unwrap_or_else(|| {
                panic!(
                    "embedded bootfs file {} disappeared during traversal",
                    entry.name()
                )
            });
        fs::write(&destination, file.object().contents()).map_err(|source| {
            MountError::WriteFile {
                path: destination,
                source,
            }
        })?;
    }

    Ok(())
}

fn merge_host_tree(source_root: &Path, destination_root: &Path) -> Result<(), MountError> {
    let metadata = fs::metadata(source_root).map_err(|source| MountError::ReadMetadata {
        path: source_root.to_path_buf(),
        source,
    })?;
    assert!(
        metadata.is_dir(),
        "hosted init overlay root {} is not a directory",
        source_root.display()
    );

    copy_host_directory(source_root, destination_root)
}

fn copy_host_directory(source_root: &Path, destination_root: &Path) -> Result<(), MountError> {
    fs::create_dir_all(destination_root).map_err(|source| MountError::CreateDirectory {
        path: destination_root.to_path_buf(),
        source,
    })?;

    for entry in fs::read_dir(source_root).map_err(|source| MountError::ReadDirectory {
        path: source_root.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| MountError::ReadDirectory {
            path: source_root.to_path_buf(),
            source,
        })?;
        let source_path = entry.path();
        let destination_path = destination_root.join(entry.file_name());
        let metadata = entry
            .metadata()
            .map_err(|source| MountError::ReadMetadata {
                path: source_path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            if destination_path.is_file() {
                return Err(MountError::Collision {
                    path: destination_path,
                });
            }

            copy_host_directory(&source_path, &destination_path)?;
            continue;
        }

        if destination_path.exists() {
            return Err(MountError::Collision {
                path: destination_path,
            });
        }

        fs::copy(&source_path, &destination_path).map_err(|source| MountError::WriteFile {
            path: destination_path,
            source,
        })?;
    }

    Ok(())
}
