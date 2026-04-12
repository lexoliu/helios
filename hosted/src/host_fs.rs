use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use helios_kernel::{HostDirEntry, HostFileSystem, HostFsError, HostMetadata};

/// Host-OS backed filesystem for the hosted backend.
///
/// Maps all paths relative to a root directory on the host machine. This
/// enables kernel `component_wasi` to serve real host filesystem content
/// through the same interface that bare-metal backends use over virtio-9p.
#[derive(Clone)]
pub struct HostedFileSystem {
    root: PathBuf,
}

impl HostedFileSystem {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let cleaned = path.strip_prefix('/').unwrap_or(path);
        self.root.join(cleaned)
    }
}

fn map_io_error(error: io::Error) -> HostFsError {
    use helios_hal::io::IoError;
    let io_error = match error.kind() {
        io::ErrorKind::NotFound => IoError::NotFound,
        io::ErrorKind::AlreadyExists => IoError::AlreadyExists,
        io::ErrorKind::PermissionDenied => IoError::PermissionDenied,
        io::ErrorKind::Unsupported => IoError::Unsupported,
        _ => IoError::DeviceFault,
    };
    HostFsError::Transport(io_error)
}

impl HostFileSystem for HostedFileSystem {
    type StatFuture<'a> = core::future::Ready<Result<HostMetadata, HostFsError>>;
    type ReadDirFuture<'a> = core::future::Ready<Result<Vec<HostDirEntry>, HostFsError>>;
    type ReadFileFuture<'a> = core::future::Ready<Result<Vec<u8>, HostFsError>>;
    type ReadFileRangeFuture<'a> = core::future::Ready<Result<Vec<u8>, HostFsError>>;
    type WriteFileFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type TruncateFileFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type CreateFileFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type CreateDirectoryFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type RemoveFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type RenameFuture<'a> = core::future::Ready<Result<(), HostFsError>>;

    fn stat_path(&self, path: &str) -> Self::StatFuture<'_> {
        core::future::ready(stat_impl(&self.resolve(path)))
    }

    fn read_dir(&self, path: &str) -> Self::ReadDirFuture<'_> {
        core::future::ready(read_dir_impl(&self.resolve(path)))
    }

    fn read_file(&self, path: &str) -> Self::ReadFileFuture<'_> {
        core::future::ready(fs::read(self.resolve(path)).map_err(map_io_error))
    }

    fn read_file_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Self::ReadFileRangeFuture<'_> {
        core::future::ready(read_file_range_impl(
            &self.resolve(path),
            offset,
            max_bytes,
        ))
    }

    fn write_file(&self, path: &str, offset: u64, bytes: &[u8]) -> Self::WriteFileFuture<'_> {
        core::future::ready(write_file_impl(&self.resolve(path), offset, bytes))
    }

    fn truncate_file(&self, path: &str) -> Self::TruncateFileFuture<'_> {
        core::future::ready(truncate_impl(&self.resolve(path)))
    }

    fn create_file(&self, path: &str) -> Self::CreateFileFuture<'_> {
        core::future::ready(
            fs::File::create_new(self.resolve(path))
                .map(|_| ())
                .map_err(map_io_error),
        )
    }

    fn create_directory(&self, path: &str) -> Self::CreateDirectoryFuture<'_> {
        core::future::ready(fs::create_dir(self.resolve(path)).map_err(map_io_error))
    }

    fn remove(&self, path: &str, directory: bool) -> Self::RemoveFuture<'_> {
        let resolved = self.resolve(path);
        core::future::ready(if directory {
            fs::remove_dir(resolved).map_err(map_io_error)
        } else {
            fs::remove_file(resolved).map_err(map_io_error)
        })
    }

    fn rename(&self, source: &str, destination: &str) -> Self::RenameFuture<'_> {
        core::future::ready(
            fs::rename(self.resolve(source), self.resolve(destination)).map_err(map_io_error),
        )
    }
}

fn stat_impl(path: &Path) -> Result<HostMetadata, HostFsError> {
    let meta = fs::metadata(path).map_err(map_io_error)?;
    let qid_type = if meta.is_dir() { 0x80 } else { 0x00 };
    let mode = if meta.is_dir() { 0o040755 } else { 0o100644 };
    Ok(HostMetadata {
        qid_path: 0,
        qid_type,
        mode,
        size: meta.len(),
    })
}

fn read_dir_impl(path: &Path) -> Result<Vec<HostDirEntry>, HostFsError> {
    let entries = fs::read_dir(path).map_err(map_io_error)?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(map_io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .unwrap_or_else(|os| os.to_string_lossy().into_owned());
        let is_directory = entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or(false);
        result.push(HostDirEntry { name, is_directory });
    }
    Ok(result)
}

fn read_file_range_impl(
    path: &Path,
    offset: u64,
    max_bytes: u32,
) -> Result<Vec<u8>, HostFsError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).map_err(map_io_error)?;
    file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
    let mut buf = vec![0u8; max_bytes as usize];
    let n = file.read(&mut buf).map_err(map_io_error)?;
    buf.truncate(n);
    Ok(buf)
}

fn write_file_impl(path: &Path, offset: u64, bytes: &[u8]) -> Result<(), HostFsError> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(map_io_error)?;
    file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
    file.write_all(bytes).map_err(map_io_error)?;
    Ok(())
}

fn truncate_impl(path: &Path) -> Result<(), HostFsError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(map_io_error)?;
    file.set_len(0).map_err(map_io_error)?;
    Ok(())
}
