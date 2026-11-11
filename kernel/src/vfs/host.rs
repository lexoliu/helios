//! The host share as a mountable filesystem.
//!
//! [`HostShare`] adapts the kernel's 9p client ([`crate::HostFileSystem`])
//! to the filesystem contract. The client speaks host-relative paths whose
//! root is `/`; a [`FsPath`] is relative to the share's root already, so
//! the root maps to `/` and every other path passes through unchanged.
//!
//! Concurrency contract: the client is a cheap handle whose transport
//! serialises requests on its own; this adapter adds no state.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bytes::Bytes;
use helios_hal::fs::{
    DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Metadata, NodeId,
    NodeKind, OpenFlags,
};

use crate::{HostDirLimit, HostFileSystem, HostFsError, HostFsErrorKind, HostMetadata};

/// 9p `QTDIR`: the qid type bit that marks a directory.
const P9_QID_TYPE_DIRECTORY: u8 = 0x80;

/// The most of one host directory a listing holds in kernel memory.
///
/// The share lists a whole directory per request and pages the guest's
/// cookie over it, so the guest's buffer bounds nothing. This is a policy
/// bound, not one derived from a stated budget: it keeps a host that never
/// ends a listing from growing kernel memory without limit, while a real
/// directory of tens of thousands of names still lists.
const HOST_DIRECTORY_LIMIT: HostDirLimit = HostDirLimit {
    max_entries: 65_536,
    max_name_bytes: 4 * 1024 * 1024,
};

/// The host share, mounted.
#[derive(Clone)]
pub struct HostShare<H> {
    client: H,
}

impl<H> HostShare<H> {
    pub fn new(client: H) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &H {
        &self.client
    }
}

fn host_path(path: FsPath<'_>) -> &str {
    if path.is_root() { "/" } else { path.as_str() }
}

pub(crate) fn fs_error_from_host(error: HostFsError) -> FsError {
    match error.kind() {
        HostFsErrorKind::NoEntry => FsError::NoEntry,
        HostFsErrorKind::Exist => FsError::Exists,
        HostFsErrorKind::NotDirectory => FsError::NotDirectory,
        HostFsErrorKind::IsDirectory => FsError::IsDirectory,
        HostFsErrorKind::NotEmpty => FsError::NotEmpty,
        HostFsErrorKind::ReadOnly => FsError::ReadOnly,
        HostFsErrorKind::Unsupported => FsError::Unsupported,
        HostFsErrorKind::Io => FsError::Io,
    }
}

fn metadata_from_host(metadata: &HostMetadata) -> Metadata {
    Metadata {
        id: NodeId::new(
            DeviceId::new(metadata.identity.domain().raw()),
            metadata.identity.local(),
        ),
        kind: if metadata.qid_type & P9_QID_TYPE_DIRECTORY != 0 {
            NodeKind::Directory
        } else {
            NodeKind::File
        },
        link_count: metadata.link_count,
        size: metadata.size,
        access_nanos: metadata.access_nanos,
        modified_nanos: metadata.modified_nanos,
        status_nanos: metadata.status_nanos,
    }
}

impl<H: HostFileSystem> FileSystem for HostShare<H> {
    async fn stat(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        self.client
            .stat_path(host_path(path))
            .await
            .map(|metadata| metadata_from_host(&metadata))
            .map_err(fs_error_from_host)
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, _now: u64) -> FsResult<Metadata> {
        let host = host_path(path);
        match self
            .client
            .stat_path(host)
            .await
            .map_err(fs_error_from_host)
        {
            Ok(metadata) => {
                let metadata = metadata_from_host(&metadata);
                if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUSIVE) {
                    return Err(FsError::Exists);
                }
                if flags.contains(OpenFlags::DIRECTORY) && metadata.kind != NodeKind::Directory {
                    return Err(FsError::NotDirectory);
                }
                if flags.contains(OpenFlags::TRUNCATE) {
                    if metadata.kind != NodeKind::File {
                        return Err(FsError::IsDirectory);
                    }
                    self.client
                        .truncate_file(host)
                        .await
                        .map_err(fs_error_from_host)?;
                }
                Ok(metadata)
            }
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                if flags.contains(OpenFlags::DIRECTORY) {
                    return Err(FsError::Invalid);
                }
                self.client
                    .create_file(host)
                    .await
                    .map_err(fs_error_from_host)?;
                self.stat(path).await
            }
            Err(error) => Err(error),
        }
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        let max_bytes = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        self.client
            .read_file_range(host_path(path), offset, max_bytes)
            .await
            .map(Bytes::from)
            .map_err(fs_error_from_host)
    }

    async fn write_at(
        &self,
        path: FsPath<'_>,
        offset: u64,
        data: &[u8],
        _now: u64,
    ) -> FsResult<()> {
        self.client
            .write_file(host_path(path), offset, data)
            .await
            .map_err(fs_error_from_host)
    }

    async fn append(&self, path: FsPath<'_>, data: &[u8], _now: u64) -> FsResult<u64> {
        self.client
            .append_file(host_path(path), data)
            .await
            .map_err(fs_error_from_host)
    }

    async fn set_size(&self, path: FsPath<'_>, size: u64, _now: u64) -> FsResult<()> {
        self.client
            .set_file_size(host_path(path), size)
            .await
            .map_err(fs_error_from_host)
    }

    async fn set_times(
        &self,
        path: FsPath<'_>,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        _now: u64,
    ) -> FsResult<()> {
        self.client
            .set_times(host_path(path), access_nanos, modified_nanos)
            .await
            .map_err(fs_error_from_host)
    }

    async fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        // 9p lists a whole directory per request; the cookie is the
        // position in that listing.
        let skip = usize::try_from(cookie.raw()).map_err(|_| FsError::Invalid)?;
        let entries = self
            .client
            .read_dir(host_path(path), HOST_DIRECTORY_LIMIT)
            .await
            .map_err(fs_error_from_host)?;
        Ok(entries
            .into_iter()
            .enumerate()
            .skip(skip)
            .take(max_entries)
            .map(|(position, entry)| DirEntry {
                name: entry.name,
                kind: if entry.is_directory {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                },
                inode: 0,
                next: DirCookie::new(position as u64 + 1),
            })
            .collect())
    }

    async fn create_directory(&self, path: FsPath<'_>, _now: u64) -> FsResult<()> {
        self.client
            .create_directory(host_path(path))
            .await
            .map_err(fs_error_from_host)
    }

    async fn remove_directory(&self, path: FsPath<'_>) -> FsResult<()> {
        self.client
            .remove(host_path(path), true)
            .await
            .map_err(fs_error_from_host)
    }

    async fn unlink(&self, path: FsPath<'_>) -> FsResult<()> {
        self.client
            .remove(host_path(path), false)
            .await
            .map_err(fs_error_from_host)
    }

    async fn rename(&self, source: FsPath<'_>, destination: FsPath<'_>, _now: u64) -> FsResult<()> {
        self.client
            .rename(host_path(source), host_path(destination))
            .await
            .map_err(fs_error_from_host)
    }

    async fn link(&self, existing: FsPath<'_>, link: FsPath<'_>, _now: u64) -> FsResult<()> {
        self.client
            .hard_link(host_path(existing), host_path(link))
            .await
            .map_err(fs_error_from_host)
    }

    async fn symlink(&self, target: &str, link: FsPath<'_>, _now: u64) -> FsResult<()> {
        self.client
            .symlink(target, host_path(link))
            .await
            .map_err(fs_error_from_host)
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        self.client
            .read_link(host_path(path))
            .await
            .map_err(fs_error_from_host)
    }

    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        // 9p has one `Tfsync`, which the host turns into `fsync`: data and
        // metadata both, which also satisfies `sync_data`.
        self.client
            .sync_file(host_path(path))
            .await
            .map_err(fs_error_from_host)
    }

    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        self.sync(path).await
    }
}

#[cfg(test)]
mod tests {
    use helios_hal::fs::NodeKind;

    use super::{P9_QID_TYPE_DIRECTORY, metadata_from_host};
    use crate::{AuthorityDomain, HostMetadata, ObjectIdentity, object_identity};

    fn host_metadata(identity: ObjectIdentity, qid_type: u8) -> HostMetadata {
        HostMetadata {
            identity,
            qid_path: identity.local(),
            qid_type,
            mode: 0o100_644,
            size: 17,
            link_count: 2,
            access_nanos: 3_000_000_004,
            modified_nanos: 5_000_000_006,
            status_nanos: 7_000_000_008,
        }
    }

    /// A host-share stat carries the host's own timestamps, link count and
    /// identity: `make`, `rsync` and every other mtime-driven tool treat a
    /// shared file as ageless otherwise.
    #[test]
    fn host_metadata_keeps_timestamps_link_count_and_identity() {
        let identity = ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 4242);

        let metadata = metadata_from_host(&host_metadata(identity, 0));

        assert_eq!(metadata.kind, NodeKind::File);
        assert_eq!(metadata.link_count, 2);
        assert_eq!(metadata.size, 17);
        assert_eq!(metadata.access_nanos, 3_000_000_004);
        assert_eq!(metadata.modified_nanos, 5_000_000_006);
        assert_eq!(metadata.status_nanos, 7_000_000_008);
        assert_eq!(object_identity(&metadata), identity);
    }

    #[test]
    fn host_directory_metadata_is_a_directory() {
        let identity = ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 9);

        let metadata = metadata_from_host(&host_metadata(identity, P9_QID_TYPE_DIRECTORY));

        assert_eq!(metadata.kind, NodeKind::Directory);
    }
}
