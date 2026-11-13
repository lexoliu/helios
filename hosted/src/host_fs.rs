use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{self as unix_fs, FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use helios_hal::fs::{WritePosition, WriteProgress};
use helios_hal::io::IoError;
use helios_kernel::{
    AuthorityDomain, HostDirEntry, HostDirLimit, HostFileSystem, HostFilledWrite, HostFsError,
    HostMetadata, HostWrite, ObjectIdentity,
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
                Component::Prefix(_) | Component::ParentDir => {
                    return Err(HostFsError::Transport(IoError::PermissionDenied));
                }
                Component::RootDir | Component::CurDir => {}
                Component::Normal(segment) => resolved.push(segment),
            }
        }
        Ok(resolved)
    }
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

async fn blocking_path<T>(
    path: Result<PathBuf, HostFsError>,
    operation: impl FnOnce(PathBuf) -> Result<T, HostFsError> + Send + 'static,
) -> Result<T, HostFsError>
where
    T: Send + 'static,
{
    let path = path?;
    blocking::unblock(move || operation(path)).await
}

async fn blocking_two_paths<T>(
    source: Result<PathBuf, HostFsError>,
    destination: Result<PathBuf, HostFsError>,
    operation: impl FnOnce(PathBuf, PathBuf) -> Result<T, HostFsError> + Send + 'static,
) -> Result<T, HostFsError>
where
    T: Send + 'static,
{
    let source = source?;
    let destination = destination?;
    blocking::unblock(move || operation(source, destination)).await
}

impl HostFileSystem for HostedFileSystem {
    type Write<'a> = HostedWrite;

    /// The hosted backend calls the host OS directly, which does its own
    /// caching; there is no 9p session in front of it to cache for.
    fn cache_stats(&self) -> Option<helios_kernel::HostFsCacheStats> {
        None
    }

    /// The host OS is called directly: no session to drop and no cache
    /// of ours to flush.
    fn after_machine_restore(
        &self,
        _restore: helios_kernel::HostShareRestore,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        core::future::ready(())
    }

    /// The host OS is called directly: there is no 9p session for a VMM
    /// to see, so a snapshot has nothing to wait for.
    fn prepare_snapshot(&self) -> impl core::future::Future<Output = ()> + Send + '_ {
        core::future::ready(())
    }

    fn snapshot_finished(&self) {}

    fn stat_path(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<HostMetadata, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| stat_impl(&path))
    }

    fn read_dir(
        &self,
        path: &str,
        limit: HostDirLimit,
    ) -> impl core::future::Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + Send + '_
    {
        blocking_path(self.resolve(path), move |path| read_dir_impl(&path, limit))
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

    fn prepare_write(
        &self,
        path: &str,
        position: WritePosition,
        len: usize,
    ) -> impl core::future::Future<Output = Result<HostedWrite, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            open_for_write(&path, position, len)
        })
    }

    fn sync_file(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| sync_file_impl(&path))
    }

    fn truncate_file(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| set_file_size_impl(&path, 0))
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
        link_count: meta.nlink(),
        access_nanos: unix_timestamp_nanos(meta.atime(), meta.atime_nsec()),
        modified_nanos: unix_timestamp_nanos(meta.mtime(), meta.mtime_nsec()),
        status_nanos: unix_timestamp_nanos(meta.ctime(), meta.ctime_nsec()),
    })
}

/// Normalises a host `seconds`/`nanoseconds` pair to nanoseconds since the
/// Unix epoch. Pre-epoch timestamps clamp to zero: WASI timestamps are
/// unsigned, so there is no representation for them.
fn unix_timestamp_nanos(seconds: i64, subnanoseconds: i64) -> u64 {
    u64::try_from(seconds)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(subnanoseconds).unwrap_or(0))
}

fn read_dir_impl(path: &Path, limit: HostDirLimit) -> Result<Vec<HostDirEntry>, HostFsError> {
    let entries = fs::read_dir(path).map_err(map_io_error)?;
    let mut result = Vec::new();
    let mut name_bytes = 0_usize;
    for entry in entries {
        let entry = entry.map_err(map_io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .unwrap_or_else(|os| os.to_string_lossy().into_owned());
        let is_directory = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        name_bytes = name_bytes.saturating_add(name.len());
        limit.check(result.len() + 1, name_bytes)?;
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

/// A write to a host file, staged by opening it: an append opens it with
/// `O_APPEND`, so the host OS positions the bytes at its end of file when
/// they arrive.
///
/// The destination is the buffer handed to the host OS's write, and the
/// write lands in one round of its whole length: the hosted backend runs
/// the kernel for development on the host itself, with no request pool to
/// bound a round by.
pub struct HostedWrite {
    file: fs::File,
    position: WritePosition,
    bytes: Vec<u8>,
}

/// A [`HostedWrite`] whose bytes are in.
pub struct FilledHostedWrite {
    write: HostedWrite,
}

fn open_for_write(
    path: &Path,
    position: WritePosition,
    len: usize,
) -> Result<HostedWrite, HostFsError> {
    let mut options = fs::OpenOptions::new();
    match position {
        WritePosition::At(_) => options.write(true),
        WritePosition::End => options.append(true),
    };
    let file = options.open(path).map_err(map_io_error)?;
    Ok(HostedWrite {
        file,
        position,
        bytes: vec![0; len],
    })
}

impl HostWrite for HostedWrite {
    type Filled = FilledHostedWrite;

    fn staged_len(&self) -> usize {
        self.bytes.len()
    }

    fn fill<F>(mut self, mut fill: F) -> FilledHostedWrite
    where
        F: FnMut(usize, &mut [u8]),
    {
        fill(0, &mut self.bytes);
        FilledHostedWrite { write: self }
    }
}

impl HostFilledWrite for FilledHostedWrite {
    type Next = HostedWrite;

    async fn submit(self) -> Result<WriteProgress<HostedWrite>, HostFsError> {
        let HostedWrite {
            mut file,
            position,
            bytes,
        } = self.write;
        blocking::unblock(move || {
            let landed = match position {
                WritePosition::At(offset) => {
                    file.write_all_at(&bytes, offset).map_err(map_io_error)?;
                    offset
                }
                WritePosition::End => {
                    // `O_APPEND` positions the write at the end of file
                    // itself; the length read here is only reported back
                    // so callers can track the stream offset.
                    let offset = file.metadata().map_err(map_io_error)?.len();
                    file.write_all(&bytes).map_err(map_io_error)?;
                    offset
                }
            };
            Ok(WriteProgress::Done(landed))
        })
        .await
    }
}

fn sync_file_impl(path: &Path) -> Result<(), HostFsError> {
    fs::File::open(path)
        .map_err(map_io_error)?
        .sync_all()
        .map_err(map_io_error)
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

    use tempfile::TempDir;

    use helios_hal::fs::{WritePosition, WriteProgress};
    use helios_kernel::{HostFileSystem, HostFilledWrite, HostFsError, HostWrite};

    use super::HostedFileSystem;

    fn test_root() -> TempDir {
        tempfile::Builder::new()
            .prefix("helios-hosted-fs-")
            .tempdir()
            .expect("test root must be created")
    }

    fn filesystem(root: &TempDir) -> HostedFileSystem {
        HostedFileSystem::new(root.path().to_path_buf())
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
            let root = test_root();
            std::fs::write(root.path().join("source"), b"payload")
                .expect("source file must be written");
            let filesystem = filesystem(&root);

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

            let root = test_root();
            std::fs::write(root.path().join("source"), b"payload")
                .expect("source file must be written");
            let filesystem = filesystem(&root);

            filesystem
                .set_times("source", Some(1_500_000_002), Some(2_000_000_003))
                .await
                .expect("times must be set");

            let metadata =
                std::fs::metadata(root.path().join("source")).expect("metadata must be read");
            assert_eq!(metadata.atime(), 1);
            assert_eq!(metadata.atime_nsec(), 500_000_002);
            assert_eq!(metadata.mtime(), 2);
            assert_eq!(metadata.mtime_nsec(), 3);
        });
    }

    /// Appending must land at the host's current end of file even when the
    /// caller never tracked an offset, and must report where it wrote so a
    /// stream can keep its own position.
    #[test]
    fn hosted_filesystem_appends_at_the_current_end_of_file() {
        run_hosted_fs_test(async {
            let root = test_root();
            std::fs::write(root.path().join("log"), b"first").expect("seed file must be written");
            let filesystem = filesystem(&root);

            let offset = append(&filesystem, "log", b"-second")
                .await
                .expect("append must succeed");
            assert_eq!(offset, b"first".len() as u64);

            append(&filesystem, "log", b"-third")
                .await
                .expect("second append must succeed");

            let contents = std::fs::read(root.path().join("log")).expect("file must be readable");
            assert_eq!(contents, b"first-second-third");
        });
    }

    /// A positional write lands at its offset, and one prepared but
    /// dropped before its submit leaves the file as it was.
    #[test]
    fn hosted_filesystem_writes_at_an_offset_and_a_dropped_write_changes_nothing() {
        run_hosted_fs_test(async {
            let root = test_root();
            std::fs::write(root.path().join("data"), b"0123456789")
                .expect("seed file must be written");
            let filesystem = filesystem(&root);

            let staged = filesystem
                .prepare_write("data", WritePosition::At(3), 4)
                .await
                .expect("write must be staged");
            drop(staged);
            assert_eq!(
                std::fs::read(root.path().join("data")).expect("file must be readable"),
                b"0123456789"
            );

            let staged = filesystem
                .prepare_write("data", WritePosition::At(3), 4)
                .await
                .expect("write must be staged");
            assert_eq!(staged.staged_len(), 4);
            let landed = staged
                .fill(|at, destination| {
                    destination.copy_from_slice(&b"abcd"[at..at + destination.len()]);
                })
                .submit()
                .await
                .expect("write must land");
            assert!(matches!(landed, WriteProgress::Done(3)));
            assert_eq!(
                std::fs::read(root.path().join("data")).expect("file must be readable"),
                b"012abcd789"
            );
        });
    }

    /// A write to a path the host does not have fails while it is being
    /// staged, before any bytes are asked for.
    #[test]
    fn hosted_filesystem_refuses_a_write_to_a_missing_file_when_staging_it() {
        run_hosted_fs_test(async {
            let root = test_root();
            let filesystem = filesystem(&root);

            let error = filesystem
                .prepare_write("missing", WritePosition::At(0), 4)
                .await
                .map(drop)
                .expect_err("a missing file cannot be written");
            assert_eq!(error.kind(), helios_kernel::HostFsErrorKind::NoEntry);
        });
    }

    /// Appends `bytes` to `path` through the staged write, resolving with
    /// the offset they landed at.
    async fn append(
        filesystem: &HostedFileSystem,
        path: &str,
        bytes: &[u8],
    ) -> Result<u64, HostFsError> {
        let staged = filesystem
            .prepare_write(path, WritePosition::End, bytes.len())
            .await?;
        match staged
            .fill(|at, destination| {
                destination.copy_from_slice(&bytes[at..at + destination.len()]);
            })
            .submit()
            .await?
        {
            WriteProgress::Done(landed) => Ok(landed),
            WriteProgress::More(_) => panic!("a hosted write lands in one round"),
        }
    }

    /// `fd_sync` must reach the host rather than report success without
    /// flushing; a missing file has to surface as an error, which proves the
    /// call is really being made.
    #[test]
    fn hosted_filesystem_syncs_files_and_reports_missing_ones() {
        run_hosted_fs_test(async {
            let root = test_root();
            std::fs::write(root.path().join("data"), b"payload").expect("file must be written");
            let filesystem = filesystem(&root);

            filesystem
                .sync_file("data")
                .await
                .expect("sync of an existing file must succeed");

            let error = filesystem
                .sync_file("absent")
                .await
                .expect_err("sync of a missing file must fail");
            assert!(matches!(
                error,
                helios_kernel::HostFsError::Transport(helios_hal::io::IoError::NotFound)
            ));
        });
    }

    /// Stat has to carry the host's own identity, link count, and timestamps:
    /// `st_dev`/`st_ino` are derived from the identity and must never be zero.
    #[test]
    fn hosted_filesystem_stat_carries_identity_links_and_timestamps() {
        run_hosted_fs_test(async {
            let root = test_root();
            std::fs::write(root.path().join("source"), b"payload").expect("file must be written");
            std::fs::hard_link(root.path().join("source"), root.path().join("alias"))
                .expect("hard link must be created");
            let filesystem = filesystem(&root);
            filesystem
                .set_times("source", Some(1_500_000_002), Some(2_000_000_003))
                .await
                .expect("times must be set");

            let metadata = filesystem
                .stat_path("source")
                .await
                .expect("stat must succeed");

            assert_ne!(metadata.identity.domain().raw(), 0);
            assert_ne!(metadata.identity.local(), 0);
            assert_eq!(metadata.link_count, 2);
            assert_eq!(metadata.size, b"payload".len() as u64);
            assert_eq!(metadata.access_nanos, 1_500_000_002);
            assert_eq!(metadata.modified_nanos, 2_000_000_003);
            assert_ne!(metadata.status_nanos, 0);

            let alias = filesystem
                .stat_path("alias")
                .await
                .expect("alias stat must succeed");
            assert_eq!(alias.identity, metadata.identity);
        });
    }

    #[test]
    fn hosted_filesystem_rejects_parent_components() {
        run_hosted_fs_test(async {
            let root = test_root();
            let filesystem = filesystem(&root);

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
