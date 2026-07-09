use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{self as unix_fs, MetadataExt};
use std::path::{Component, Path, PathBuf};

use helios_hal::io::IoError;
use helios_kernel::{
    AuthorityDomain, HostDirEntry, HostFileSystem, HostFsError, HostMetadata, ObjectIdentity,
};

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

    fn resolve(&self, path: &str) -> Result<PathBuf, HostFsError> {
        let mut resolved = self.root.clone();
        for component in Path::new(path).components() {
            match component {
                Component::Prefix(_) | Component::ParentDir => return Err(invalid_host_path()),
                Component::RootDir | Component::CurDir => {}
                Component::Normal(segment) => resolved.push(segment),
            }
        }
        Ok(resolved)
    }
}

fn invalid_host_path() -> HostFsError {
    HostFsError::Transport(IoError::PermissionDenied)
}

fn map_io_error(error: io::Error) -> HostFsError {
    let io_error = match error.kind() {
        io::ErrorKind::NotFound => IoError::NotFound,
        io::ErrorKind::AlreadyExists => IoError::AlreadyExists,
        io::ErrorKind::PermissionDenied => IoError::PermissionDenied,
        io::ErrorKind::Unsupported => IoError::Unsupported,
        _ => IoError::DeviceFault,
    };
    HostFsError::Transport(io_error)
}

fn blocking_path<T>(
    path: Result<PathBuf, HostFsError>,
    operation: impl FnOnce(PathBuf) -> Result<T, HostFsError> + Send + 'static,
) -> impl core::future::Future<Output = Result<T, HostFsError>> + Send
where
    T: Send + 'static,
{
    async move {
        let path = path?;
        blocking::unblock(move || operation(path)).await
    }
}

fn blocking_two_paths<T>(
    source: Result<PathBuf, HostFsError>,
    destination: Result<PathBuf, HostFsError>,
    operation: impl FnOnce(PathBuf, PathBuf) -> Result<T, HostFsError> + Send + 'static,
) -> impl core::future::Future<Output = Result<T, HostFsError>> + Send
where
    T: Send + 'static,
{
    async move {
        let source = source?;
        let destination = destination?;
        blocking::unblock(move || operation(source, destination)).await
    }
}

impl HostFileSystem for HostedFileSystem {
    fn stat_path(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<HostMetadata, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| stat_impl(&path))
    }

    fn read_dir(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + Send + '_
    {
        blocking_path(self.resolve(path), |path| read_dir_impl(&path))
    }

    fn read_file(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<Vec<u8>, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| {
            fs::read(path).map_err(map_io_error)
        })
    }

    fn read_file_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> impl core::future::Future<Output = Result<Vec<u8>, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            read_file_range_impl(&path, offset, max_bytes)
        })
    }

    fn write_file(
        &self,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        let bytes = bytes.to_vec();
        blocking_path(self.resolve(path), move |path| {
            write_file_impl(&path, offset, &bytes)
        })
    }

    fn truncate_file(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| truncate_impl(&path))
    }

    fn set_file_size(
        &self,
        path: &str,
        size: u64,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            set_file_size_impl(&path, size)
        })
    }

    fn set_times(
        &self,
        path: &str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            set_times_impl(&path, access_nanos, modified_nanos)
        })
    }

    fn create_file(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| {
            fs::File::create_new(path).map(|_| ()).map_err(map_io_error)
        })
    }

    fn create_directory(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| {
            fs::create_dir(path).map_err(map_io_error)
        })
    }

    fn remove(
        &self,
        path: &str,
        directory: bool,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            if directory {
                fs::remove_dir(path).map_err(map_io_error)
            } else {
                fs::remove_file(path).map_err(map_io_error)
            }
        })
    }

    fn rename(
        &self,
        source: &str,
        destination: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_two_paths(
            self.resolve(source),
            self.resolve(destination),
            |source, destination| fs::rename(source, destination).map_err(map_io_error),
        )
    }

    fn hard_link(
        &self,
        source: &str,
        destination: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_two_paths(
            self.resolve(source),
            self.resolve(destination),
            |source, destination| fs::hard_link(source, destination).map_err(map_io_error),
        )
    }

    fn symlink(
        &self,
        target: &str,
        link_path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        let target = PathBuf::from(target);
        blocking_path(self.resolve(link_path), move |link_path| {
            unix_fs::symlink(target, link_path).map_err(map_io_error)
        })
    }

    fn read_link(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<String, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| read_link_impl(&path))
    }
}

fn stat_impl(path: &Path) -> Result<HostMetadata, HostFsError> {
    let meta = fs::metadata(path).map_err(map_io_error)?;
    let qid_type = if meta.is_dir() { 0x80 } else { 0x00 };
    let mode = if meta.is_dir() { 0o040755 } else { 0o100644 };
    let inode = meta.ino();
    Ok(HostMetadata {
        identity: ObjectIdentity::new(AuthorityDomain::host_device(meta.dev()), inode),
        qid_path: inode,
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
        let is_directory = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        result.push(HostDirEntry { name, is_directory });
    }
    Ok(result)
}

fn read_file_range_impl(path: &Path, offset: u64, max_bytes: u32) -> Result<Vec<u8>, HostFsError> {
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
    set_file_size_impl(path, 0)
}

fn set_file_size_impl(path: &Path, size: u64) -> Result<(), HostFsError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(map_io_error)?;
    file.set_len(size).map_err(map_io_error)?;
    Ok(())
}

fn set_times_impl(
    path: &Path,
    access_nanos: Option<u64>,
    modified_nanos: Option<u64>,
) -> Result<(), HostFsError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| HostFsError::Utf8)?;
    let times = [
        timespec_from_optional_nanos(access_nanos)?,
        timespec_from_optional_nanos(modified_nanos)?,
    ];
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(map_io_error(io::Error::last_os_error()))
    }
}

fn timespec_from_optional_nanos(nanos: Option<u64>) -> Result<libc::timespec, HostFsError> {
    let Some(nanos) = nanos else {
        return Ok(libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        });
    };
    let seconds = libc::time_t::try_from(nanos / 1_000_000_000)
        .map_err(|_| HostFsError::Transport(helios_hal::io::IoError::OutOfBounds))?;
    let nanoseconds = libc::c_long::try_from(nanos % 1_000_000_000)
        .map_err(|_| HostFsError::Transport(helios_hal::io::IoError::OutOfBounds))?;
    Ok(libc::timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds,
    })
}

fn read_link_impl(path: &Path) -> Result<String, HostFsError> {
    fs::read_link(path)
        .map_err(map_io_error)?
        .into_os_string()
        .into_string()
        .map_err(|_| HostFsError::Utf8)
}

#[cfg(test)]
mod tests {
    use core::future::Future;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use helios_kernel::HostFileSystem;

    use super::HostedFileSystem;

    struct TestRoot {
        path: PathBuf,
    }

    impl TestRoot {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("helios-hosted-fs-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&path).expect("test root must be created");
            Self { path }
        }

        fn filesystem(&self) -> HostedFileSystem {
            HostedFileSystem::new(self.path.clone())
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                tracing::warn!(
                    path = %self.path.display(),
                    ?error,
                    "failed to remove hosted filesystem test root"
                );
            }
        }
    }

    fn run_hosted_fs_test(test: impl Future<Output = ()> + Send + 'static) {
        tokio::runtime::Builder::new_multi_thread()
            .build()
            .expect("hosted filesystem test runtime must be created")
            .block_on(test);
    }

    #[test]
    fn hosted_filesystem_supports_links() {
        run_hosted_fs_test(async {
            let root = TestRoot::new();
            std::fs::write(root.path.join("source"), b"payload")
                .expect("source file must be written");
            let filesystem = root.filesystem();

            filesystem
                .hard_link("source", "hard")
                .await
                .expect("hard link must be created");
            filesystem
                .symlink("source", "sym")
                .await
                .expect("symlink must be created");

            let hard = filesystem
                .read_file("hard")
                .await
                .expect("hard link must be readable");
            let payload = filesystem
                .read_link("sym")
                .await
                .expect("symlink payload must be readable");

            assert_eq!(&hard, b"payload");
            assert_eq!(payload, "source");
        });
    }

    #[test]
    fn hosted_filesystem_sets_access_and_modification_times() {
        run_hosted_fs_test(async {
            use std::os::unix::fs::MetadataExt;

            let root = TestRoot::new();
            std::fs::write(root.path.join("source"), b"payload")
                .expect("source file must be written");
            let filesystem = root.filesystem();

            filesystem
                .set_times("source", Some(1_500_000_002), Some(2_000_000_003))
                .await
                .expect("times must be set");

            let metadata =
                std::fs::metadata(root.path.join("source")).expect("metadata must be read");
            assert_eq!(metadata.atime(), 1);
            assert_eq!(metadata.atime_nsec(), 500_000_002);
            assert_eq!(metadata.mtime(), 2);
            assert_eq!(metadata.mtime_nsec(), 3);
        });
    }

    #[test]
    fn hosted_filesystem_rejects_parent_components() {
        run_hosted_fs_test(async {
            let root = TestRoot::new();
            let filesystem = root.filesystem();

            let error = filesystem
                .read_file("../outside")
                .await
                .expect_err("parent path must be rejected");

            assert!(matches!(
                error,
                helios_kernel::HostFsError::Transport(helios_hal::io::IoError::PermissionDenied)
            ));
        });
    }
}
