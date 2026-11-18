use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{self as unix_fs, FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use helios_hal::fs::{Landed, NodeKind, WritePosition, WriteProgress};
use helios_hal::io::IoError;
use helios_kernel::{
    AuthorityDomain, HostCreation, HostDirEntry, HostDirLimit, HostFileSystem, HostFilledWrite,
    HostFsError, HostMetadata, HostWrite, ObjectIdentity,
};

/// Linux `ENOSPC`, the code a host-share server answers a full device with.
const ENOSPC: u32 = 28;

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
    if error.kind() == io::ErrorKind::StorageFull {
        // The host-share error a 9p server's `ENOSPC` reply becomes.
        return HostFsError::Server(ENOSPC);
    }
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
    type Write = HostedWrite;

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
            require_openable(&path)?;
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
        creation: HostCreation,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            let file = fs::File::create_new(&path).map_err(map_io_error)?;
            if file.metadata().map_err(map_io_error)?.gid() != creation.gid {
                unix_fs::fchown(&file, None, Some(creation.gid)).map_err(map_io_error)?;
            }
            file.set_permissions(fs::Permissions::from_mode(creation.permissions))
                .map_err(map_io_error)
        })
    }

    fn create_directory(
        &self,
        path: &str,
        creation: HostCreation,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), move |path| {
            fs::create_dir(&path).map_err(map_io_error)?;
            in_group(&path, creation.gid)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(creation.permissions))
                .map_err(map_io_error)
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
        gid: u32,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        let target = PathBuf::from(target);
        blocking_path(self.resolve(link_path), move |link_path| {
            unix_fs::symlink(target, &link_path).map_err(map_io_error)?;
            in_group(&link_path, gid)
        })
    }

    fn read_link(
        &self,
        path: &str,
    ) -> impl core::future::Future<Output = Result<String, HostFsError>> + Send + '_ {
        blocking_path(self.resolve(path), |path| read_link_impl(&path))
    }
}

/// Puts the node at `path` in group `gid`, changing nothing when the host
/// already did (a BSD host gives a new node its directory's group). Run
/// before setting permission bits so `chown` never strips a set-id bit.
fn in_group(path: &Path, gid: u32) -> Result<(), HostFsError> {
    let current = fs::symlink_metadata(path).map_err(map_io_error)?.gid();
    if current != gid {
        unix_fs::lchown(path, None, Some(gid)).map_err(map_io_error)?;
    }
    Ok(())
}

/// The attributes of the object `path` names, itself and not what a
/// symbolic link there points at: the kernel resolves links through
/// `read_link`, as the 9p share's `Tgetattr` does.
fn stat_impl(path: &Path) -> Result<HostMetadata, HostFsError> {
    let meta = fs::symlink_metadata(path).map_err(map_io_error)?;
    let mode = meta.mode();
    // 9p `QTDIR` and `QTSYMLINK`.
    let file_type = meta.file_type();
    let qid_type = if file_type.is_dir() {
        0x80
    } else if file_type.is_symlink() {
        0x02
    } else {
        0x00
    };
    let inode = meta.ino();
    Ok(HostMetadata {
        identity: ObjectIdentity::new(AuthorityDomain::host_device(meta.dev()), inode),
        qid_path: inode,
        qid_type,
        mode,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev(),
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
        let kind = entry.file_type().ok().map(|file_type| {
            if file_type.is_dir() {
                NodeKind::Directory
            } else if file_type.is_symlink() {
                NodeKind::Symlink
            } else if file_type.is_file() {
                NodeKind::File
            } else if file_type.is_char_device() {
                NodeKind::CharacterDevice
            } else if file_type.is_block_device() {
                NodeKind::BlockDevice
            } else if file_type.is_fifo() {
                NodeKind::Fifo
            } else {
                NodeKind::Socket
            }
        });
        name_bytes = name_bytes.saturating_add(name.len());
        limit.check(result.len() + 1, name_bytes)?;
        result.push(HostDirEntry { name, kind });
    }
    Ok(result)
}

fn read_file_range_impl(path: &Path, offset: u64, max_bytes: u32) -> Result<Vec<u8>, HostFsError> {
    use std::io::{Read, Seek, SeekFrom};
    require_openable(path)?;
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
    require_openable(path)?;
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
        let len = bytes.len();
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
            Ok(WriteProgress::Done(Landed {
                offset: landed,
                len,
            }))
        })
        .await
    }
}

/// Refuses, before opening it, a device, FIFO, socket or symbolic link:
/// the host would open it with this process's privileges.
fn require_openable(path: &Path) -> Result<(), HostFsError> {
    stat_impl(path)?.require_openable()
}

fn sync_file_impl(path: &Path) -> Result<(), HostFsError> {
    require_openable(path)?;
    fs::File::open(path)
        .map_err(map_io_error)?
        .sync_all()
        .map_err(map_io_error)
}

fn set_file_size_impl(path: &Path, size: u64) -> Result<(), HostFsError> {
    require_openable(path)?;
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
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
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

    use helios_hal::fs::{Landed, WritePosition, WriteProgress};
    use helios_kernel::{HostDirLimit, HostFileSystem, HostFilledWrite, HostFsError, HostWrite};

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

    /// The group the test root has, which every node made in it can take.
    fn current_gid(root: &TempDir) -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(root.path())
            .expect("the test root has metadata")
            .gid()
    }

    /// A node the share creates takes its group and mode from the
    /// directory it is made in, with the execute bits cleared for a
    /// regular file, whatever the process's umask. Its owner is the host's
    /// to give, here the user that owns the directory too.
    #[test]
    fn a_node_created_on_the_share_inherits_from_its_parent() {
        run_hosted_fs_test(async {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};

            use helios_hal::fs::{FileSystem, FsPath, OpenFlags};
            use helios_kernel::HostShare;

            let root = test_root();
            let gid = current_gid(&root);
            let share = HostShare::new(filesystem(&root));
            for (directory, mode, file_mode, child_mode) in [
                ("中文目录", 0o2750, 0o640, 0o2750),
                ("日本語のディレクトリ", 0o777, 0o666, 0o777),
                ("한국어디렉터리", 0o1755, 0o644, 0o755),
            ] {
                let path = root.path().join(directory);
                std::fs::create_dir(&path).expect("directory must be created");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .expect("directory mode must be set");
                let at = |name: &str| format!("{directory}/{name}");
                let file = at("ファイル");
                share
                    .open(
                        FsPath::new(&file).unwrap(),
                        OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                        0,
                    )
                    .await
                    .expect("file must be created");
                let child = at("하위");
                share
                    .create_directory(FsPath::new(&child).unwrap(), 0)
                    .await
                    .expect("directory must be created");
                let link = at("链接");
                share
                    .symlink("ファイル", FsPath::new(&link).unwrap(), 0)
                    .await
                    .expect("link must be created");
                let parent = std::fs::metadata(&path).expect("parent metadata");
                for (name, expected) in [(&file, file_mode), (&child, child_mode)] {
                    let made = std::fs::symlink_metadata(root.path().join(name))
                        .expect("created node metadata");
                    assert_eq!(made.mode() & 0o7777, expected, "{name}");
                    assert_eq!((made.uid(), made.gid()), (parent.uid(), gid), "{name}");
                }
                let made = std::fs::symlink_metadata(root.path().join(&link)).unwrap();
                assert_eq!((made.uid(), made.gid()), (parent.uid(), gid), "{link}");
            }
        });
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
                .symlink("source", "sym", current_gid(&root))
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

    #[test]
    fn hosted_filesystem_set_times_acts_on_symlink_itself() {
        run_hosted_fs_test(async {
            use std::os::unix::fs::MetadataExt;

            let root = test_root();
            std::fs::write(root.path().join("target"), b"payload").expect("target must be written");
            std::os::unix::fs::symlink("target", root.path().join("sym"))
                .expect("symlink must be created");
            let filesystem = filesystem(&root);

            filesystem
                .set_times("target", Some(10_000_000_001), Some(20_000_000_002))
                .await
                .expect("target times must be set");
            filesystem
                .set_times("sym", Some(30_000_000_003), Some(40_000_000_004))
                .await
                .expect("symlink times must be set");

            let target =
                std::fs::symlink_metadata(root.path().join("target")).expect("target stat");
            assert_eq!(target.atime(), 10);
            assert_eq!(target.atime_nsec(), 1);
            assert_eq!(target.mtime(), 20);
            assert_eq!(target.mtime_nsec(), 2);

            let link = std::fs::symlink_metadata(root.path().join("sym")).expect("symlink stat");
            assert_eq!(link.atime(), 30);
            assert_eq!(link.atime_nsec(), 3);
            assert_eq!(link.mtime(), 40);
            assert_eq!(link.mtime_nsec(), 4);
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
            assert!(matches!(
                landed,
                WriteProgress::Done(Landed { offset: 3, len: 4 })
            ));
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

    /// A FIFO in the share is listed and statted as one, and opening it
    /// for any data path is refused without opening it (opening a FIFO
    /// with no peer would block the host thread).
    #[test]
    fn hosted_filesystem_reports_a_fifo_and_refuses_to_open_it() {
        run_hosted_fs_test(async {
            let root = test_root();
            std::fs::write(root.path().join("plain"), b"data").expect("file must be written");
            let fifo = std::ffi::CString::new(
                root.path()
                    .join("pipe")
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
            )
            .expect("path has no NUL");
            // SAFETY: `fifo` is a NUL-terminated path that outlives the call.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
            std::os::unix::fs::symlink("plain", root.path().join("link"))
                .expect("symlink must be created");
            let filesystem = filesystem(&root);

            let metadata = filesystem.stat_path("pipe").await.expect("stat the FIFO");
            assert_eq!(
                metadata.kind().expect("a kind"),
                helios_hal::fs::NodeKind::Fifo
            );
            let link = filesystem.stat_path("link").await.expect("stat the link");
            assert_eq!(
                link.kind().expect("a kind"),
                helios_hal::fs::NodeKind::Symlink
            );

            let mut listed: Vec<_> = filesystem
                .read_dir(
                    "/",
                    HostDirLimit {
                        max_entries: 64,
                        max_name_bytes: 4096,
                    },
                )
                .await
                .expect("the directory lists")
                .into_iter()
                .map(|entry| (entry.name, entry.kind))
                .collect();
            listed.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                listed,
                [
                    ("link".to_owned(), Some(helios_hal::fs::NodeKind::Symlink)),
                    ("pipe".to_owned(), Some(helios_hal::fs::NodeKind::Fifo)),
                    ("plain".to_owned(), Some(helios_hal::fs::NodeKind::File)),
                ]
            );

            let refusal =
                helios_kernel::HostFsErrorKind::Refused(helios_hal::fs::FsError::Unsupported);
            let error = filesystem.read_file("pipe").await.expect_err("refused");
            assert_eq!(error.kind(), refusal);
            let error = filesystem
                .read_file_range("pipe", 0, 4)
                .await
                .expect_err("refused");
            assert_eq!(error.kind(), refusal);
            let error = filesystem
                .prepare_write("pipe", WritePosition::At(0), 4)
                .await
                .map(drop)
                .expect_err("refused");
            assert_eq!(error.kind(), refusal);
            let error = filesystem.sync_file("pipe").await.expect_err("refused");
            assert_eq!(error.kind(), refusal);
            // The entry itself is still removable.
            filesystem.remove("pipe", false).await.expect("unlink");

            // A link is refused for data too: the kernel follows it itself.
            let error = filesystem.read_file("link").await.expect_err("refused");
            assert_eq!(
                error.kind(),
                helios_kernel::HostFsErrorKind::Refused(helios_hal::fs::FsError::Loop)
            );
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
            WriteProgress::Done(landed) => Ok(landed.offset),
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
    fn hosted_filesystem_preserves_set_id_sticky_bits_and_special_nodes() {
        run_hosted_fs_test(async {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            use std::os::unix::net::UnixListener;

            let root = test_root();
            let host_meta = std::fs::metadata(root.path()).expect("root metadata");
            for (name, bits) in [
                ("suid", 0o4755),
                ("sgid", 0o2755),
                ("both", 0o6711),
                ("sticky", 0o1755),
            ] {
                let path = root.path().join(name);
                std::fs::write(&path, b"#!/bin/sh\n").expect("file must be written");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bits))
                    .expect("set-id and sticky bits must be set");
            }
            let dir = root.path().join("sgid-dir");
            std::fs::create_dir(&dir).expect("dir must be created");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o3775))
                .expect("dir setgid and sticky bits must be set");
            let sock_path = root.path().join("sock");
            let _listener = UnixListener::bind(&sock_path).expect("unix socket must bind");

            let filesystem = filesystem(&root);
            filesystem
                .create_file(
                    "sibling",
                    helios_kernel::HostCreation {
                        permissions: 0o640,
                        gid: host_meta.gid(),
                    },
                )
                .await
                .expect("sibling created");
            append(&filesystem, "sibling", b"unrelated write")
                .await
                .expect("sibling write");

            for (name, bits) in [
                ("suid", 0o104_755),
                ("sgid", 0o102_755),
                ("both", 0o106_711),
                ("sticky", 0o101_755),
                ("sgid-dir", 0o043_775),
            ] {
                let meta = filesystem.stat_path(name).await.expect("stat must succeed");
                assert_eq!(meta.mode, bits, "{name}: {:#o}", meta.mode);
                assert_eq!(
                    (meta.uid, meta.gid),
                    (host_meta.uid(), host_meta.gid()),
                    "{name}"
                );
            }
            let sock = filesystem.stat_path("sock").await.expect("stat socket");
            assert_eq!(sock.kind().unwrap(), helios_hal::fs::NodeKind::Socket);
            let refusal =
                helios_kernel::HostFsErrorKind::Refused(helios_hal::fs::FsError::Unsupported);
            assert_eq!(
                filesystem.read_file("sock").await.unwrap_err().kind(),
                refusal
            );
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
