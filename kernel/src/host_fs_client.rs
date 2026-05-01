extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use helios_hal::io::IoError;

use crate::{
    AuthorityDomain, HostDirEntry, HostFileSystem, HostFsError, HostMetadata, ObjectIdentity,
};

const DEFAULT_MSIZE: u32 = (1024 * 1024) + 24;
const P9_NOTAG: u16 = u16::MAX;
const P9_NOFID: u32 = u32::MAX;
const P9_NOUID: u32 = u32::MAX;
const P9_TVERSION: u8 = 100;
const P9_TATTACH: u8 = 104;
const P9_TWALK: u8 = 110;
const P9_TREAD: u8 = 116;
const P9_TWRITE: u8 = 118;
const P9_TCLUNK: u8 = 120;
const P9_TLOPEN: u8 = 12;
const P9_TLCREATE: u8 = 14;
const P9_TSYMLINK: u8 = 16;
const P9_TREADLINK: u8 = 22;
const P9_TGETATTR: u8 = 24;
const P9_TREADDIR: u8 = 40;
const P9_TLINK: u8 = 70;
const P9_TMKDIR: u8 = 72;
const P9_TRENAMEAT: u8 = 74;
const P9_TUNLINKAT: u8 = 76;
const P9_RLERROR: u8 = 7;
const P9_DOTL_RDONLY: u32 = 0;
const P9_DOTL_WRONLY: u32 = 1;
const P9_DOTL_TRUNC: u32 = 0o1000;
const P9_DOTL_DIRECTORY: u32 = 0o200000;
const P9_DOTL_AT_REMOVEDIR: u32 = 0x200;
const P9_STATS_BASIC: u64 = 0x0000_07ff;
const P9_QTDIR: u8 = 0x80;
const P9_WRITE_CHUNK: usize = (DEFAULT_MSIZE as usize) - 24;

pub trait HostFsTransport: Clone + Send + Sync + 'static {
    fn mount_tag(&self) -> &str;

    fn request(
        &self,
        bytes: Vec<u8>,
        response_len: usize,
    ) -> impl Future<Output = Result<Vec<u8>, IoError>> + Send + '_;
}

#[derive(Clone)]
pub struct HostFsClient<Transport: HostFsTransport> {
    transport: Transport,
}

impl<Transport: HostFsTransport> HostFsClient<Transport> {
    pub const fn new(transport: Transport) -> Self {
        Self { transport }
    }

    async fn transact(
        &self,
        ty: u8,
        body: impl FnOnce(&mut Vec<u8>),
        response_len: usize,
    ) -> Result<Vec<u8>, HostFsError> {
        let mut request = Vec::with_capacity(7 + 256);
        request.extend_from_slice(&0_u32.to_le_bytes());
        request.push(ty);
        request.extend_from_slice(&P9_NOTAG.to_le_bytes());
        body(&mut request);
        let size =
            u32::try_from(request.len()).map_err(|_| HostFsError::Protocol("request too large"))?;
        request[..4].copy_from_slice(&size.to_le_bytes());

        let response = self
            .transport
            .request(request, response_len)
            .await
            .map_err(HostFsError::Transport)?;
        if response.len() < 7 {
            return Err(HostFsError::Protocol("response shorter than 9p header"));
        }
        let actual_size = read_u32_le(&response, 0)? as usize;
        if actual_size != response.len() {
            return Err(HostFsError::Protocol("response length header mismatch"));
        }

        let response_ty = response[4];
        if response_ty == P9_RLERROR {
            return Err(HostFsError::Server(read_u32_le(&response, 7)?));
        }

        let expected = ty
            .checked_add(1)
            .ok_or(HostFsError::Protocol("response type overflowed"))?;
        if response_ty != expected {
            return Err(HostFsError::Protocol("unexpected 9p response type"));
        }

        Ok(response)
    }

    async fn attach_root(&self) -> Result<u32, HostFsError> {
        self.version().await?;
        let fid = 0;
        let mount_tag = self.transport.mount_tag().to_owned();
        let _ = self
            .transact(
                P9_TATTACH,
                move |body| {
                    push_u32(body, fid);
                    push_u32(body, P9_NOFID);
                    push_string(body, "root");
                    push_string(body, &mount_tag);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await?;
        Ok(fid)
    }

    async fn version(&self) -> Result<(), HostFsError> {
        let _ = self
            .transact(
                P9_TVERSION,
                |body| {
                    push_u32(body, DEFAULT_MSIZE);
                    push_string(body, "9P2000.L");
                },
                64,
            )
            .await?;
        Ok(())
    }

    async fn walk(&self, parent_fid: u32, new_fid: u32, path: &str) -> Result<u32, HostFsError> {
        let segments = split_path(path);
        let response = self
            .transact(
                P9_TWALK,
                |body| {
                    push_u32(body, parent_fid);
                    push_u32(body, new_fid);
                    push_u16(
                        body,
                        u16::try_from(segments.len()).expect("walk segment count overflowed u16"),
                    );
                    for segment in &segments {
                        push_string(body, segment);
                    }
                },
                4096,
            )
            .await?;
        let cursor = 7;
        let walked = read_u16_le(&response, cursor)?;
        if usize::from(walked) != segments.len() {
            // The server walked part of the path; the remaining segment
            // doesn't exist. 9p leaves `new_fid` unattached in this case
            // per the 2000.L spec ("if any walked element doesn't exist
            // the final fid is not set"), so there is nothing to clunk.
            return Err(HostFsError::Transport(helios_hal::io::IoError::NotFound));
        }
        Ok(new_fid)
    }

    async fn get_attr(&self, fid: u32) -> Result<HostMetadata, HostFsError> {
        let response = self
            .transact(
                P9_TGETATTR,
                |body| {
                    push_u32(body, fid);
                    push_u64(body, P9_STATS_BASIC);
                },
                160,
            )
            .await?;
        let mut cursor = 7;
        let _mask = read_u64_le(&response, cursor)?;
        cursor += 8;
        let qid_type = read_u8(&response, cursor)?;
        cursor += 1;
        let _qid_version = read_u32_le(&response, cursor)?;
        cursor += 4;
        let qid_path = read_u64_le(&response, cursor)?;
        cursor += 8;
        let mode = read_u32_le(&response, cursor)?;
        cursor += 4;
        cursor += 4;
        cursor += 4;
        cursor += 8;
        cursor += 8;
        let size = read_u64_le(&response, cursor)?;
        Ok(HostMetadata {
            identity: ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, qid_path),
            qid_path,
            qid_type,
            mode,
            size,
        })
    }

    async fn read_chunk(
        &self,
        fid: u32,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, HostFsError> {
        let response = self
            .transact(
                P9_TREAD,
                |body| {
                    push_u32(body, fid);
                    push_u64(body, offset);
                    push_u32(body, max_bytes);
                },
                usize::try_from(DEFAULT_MSIZE).expect("DEFAULT_MSIZE overflowed usize"),
            )
            .await?;
        let mut cursor = 7;
        let count = usize::try_from(read_u32_le(&response, cursor)?)
            .map_err(|_| HostFsError::Protocol("read payload count overflowed usize"))?;
        cursor += 4;
        Ok(read_slice(&response, cursor, count)?.to_vec())
    }

    async fn read_file_all(&self, fid: u32, expected_size: u64) -> Result<Vec<u8>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY).await?;

        let expected_size = usize::try_from(expected_size)
            .map_err(|_| HostFsError::Protocol("file size overflowed usize"))?;
        let mut bytes = Vec::with_capacity(expected_size.min(P9_WRITE_CHUNK));
        let mut offset = 0_u64;
        while bytes.len() < expected_size {
            let remaining = expected_size - bytes.len();
            let request_count = remaining.min(P9_WRITE_CHUNK);
            let request_count =
                u32::try_from(request_count).expect("read request count overflowed u32");
            let chunk = self.read_chunk(fid, offset, request_count).await?;
            if chunk.is_empty() {
                return Err(HostFsError::Protocol("short 9p read"));
            }
            offset = offset
                .checked_add(u64::try_from(chunk.len()).expect("chunk length overflowed u64"))
                .ok_or(HostFsError::Protocol("read offset overflowed u64"))?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn read_dir_entries(&self, fid: u32) -> Result<Vec<HostDirEntry>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY | P9_DOTL_DIRECTORY).await?;

        let mut entries = Vec::new();
        let mut offset = 0_u64;
        loop {
            let response = self
                .transact(
                    P9_TREADDIR,
                    |body| {
                        push_u32(body, fid);
                        push_u64(body, offset);
                        push_u32(body, DEFAULT_MSIZE.saturating_sub(24));
                    },
                    usize::try_from(DEFAULT_MSIZE).expect("DEFAULT_MSIZE overflowed usize"),
                )
                .await?;
            let mut cursor = 7;
            let count = usize::try_from(read_u32_le(&response, cursor)?)
                .map_err(|_| HostFsError::Protocol("readdir payload count overflowed usize"))?;
            cursor += 4;
            if count == 0 {
                break;
            }
            let end = cursor + count;
            while cursor < end {
                let qid_type = read_u8(&response, cursor)?;
                cursor += 1;
                cursor += 4;
                cursor += 8;
                offset = read_u64_le(&response, cursor)?;
                cursor += 8;
                let type_ = read_u8(&response, cursor)?;
                cursor += 1;
                let name = read_string(&response, &mut cursor)?;
                if name == "." || name == ".." {
                    continue;
                }
                entries.push(HostDirEntry {
                    name,
                    is_directory: qid_type & P9_QTDIR != 0 || type_ == 4,
                });
            }
        }
        Ok(entries)
    }

    async fn clunk(&self, fid: u32) -> Result<(), HostFsError> {
        let _ = self
            .transact(P9_TCLUNK, |body| push_u32(body, fid), 16)
            .await?;
        Ok(())
    }

    async fn open(&self, fid: u32, flags: u32) -> Result<(), HostFsError> {
        let _ = self
            .transact(
                P9_TLOPEN,
                |body| {
                    push_u32(body, fid);
                    push_u32(body, flags);
                },
                64,
            )
            .await?;
        Ok(())
    }

    async fn write_chunks(
        &self,
        fid: u32,
        mut offset: u64,
        mut bytes: &[u8],
    ) -> Result<(), HostFsError> {
        while !bytes.is_empty() {
            let count = bytes.len().min(P9_WRITE_CHUNK);
            let chunk = &bytes[..count];
            let response = self
                .transact(
                    P9_TWRITE,
                    |body| {
                        push_u32(body, fid);
                        push_u64(body, offset);
                        push_u32(
                            body,
                            u32::try_from(count).expect("write chunk length overflowed u32"),
                        );
                        body.extend_from_slice(chunk);
                    },
                    32,
                )
                .await?;
            let written = usize::try_from(read_u32_le(&response, 7)?)
                .map_err(|_| HostFsError::Protocol("write count overflowed usize"))?;
            if written != count {
                return Err(HostFsError::Protocol("short 9p write"));
            }
            offset = offset
                .checked_add(u64::try_from(written).expect("written length overflowed u64"))
                .ok_or(HostFsError::Protocol("write offset overflowed u64"))?;
            bytes = &bytes[written..];
        }
        Ok(())
    }

    async fn stat_path_impl(&self, path: &str) -> Result<HostMetadata, HostFsError> {
        let fid = self.attach_root().await?;
        if path == "/" {
            let metadata = self.get_attr(fid).await;
            let _ = self.clunk(fid).await;
            return metadata;
        }
        let target = self.walk(fid, 1, path).await?;
        let metadata = self.get_attr(target).await;
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        metadata
    }

    async fn read_dir_impl(&self, path: &str) -> Result<Vec<HostDirEntry>, HostFsError> {
        let fid = self.attach_root().await?;
        if path == "/" {
            let entries = self.read_dir_entries(fid).await;
            let _ = self.clunk(fid).await;
            return entries;
        }
        let target = self.walk(fid, 1, path).await?;
        let entries = self.read_dir_entries(target).await;
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        entries
    }

    async fn read_file_impl(&self, path: &str) -> Result<Vec<u8>, HostFsError> {
        let fid = self.attach_root().await?;
        let target = self.walk(fid, 1, path).await?;
        let metadata = self.get_attr(target).await;
        let data = match metadata {
            Ok(metadata) => self.read_file_all(target, metadata.size).await,
            Err(error) => Err(error),
        };
        if let Err(error) = &data {
            tracing::warn!("host-fs read_file({path}) failed: {error:?}");
        }
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        data
    }

    async fn read_file_range_impl(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, HostFsError> {
        let fid = self.attach_root().await?;
        let target = self.walk(fid, 1, path).await?;
        let data = match self.open(target, P9_DOTL_RDONLY).await {
            Ok(()) => self.read_chunk(target, offset, max_bytes).await,
            Err(error) => Err(error),
        };
        if let Err(error) = &data {
            tracing::warn!(
                "host-fs read_file_range(path={path}, offset={offset}, max_bytes={max_bytes}) failed: {error:?}"
            );
        }
        let _ = self.clunk(target).await;
        let _ = self.clunk(fid).await;
        data
    }

    async fn create_directory_impl(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let root = self.attach_root().await?;
        let directory = self.walk(root, 1, parent).await?;
        let result = self
            .transact(
                P9_TMKDIR,
                |body| {
                    push_u32(body, directory);
                    push_string(body, name);
                    push_u32(body, 0o755);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(directory).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn create_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let root = self.attach_root().await?;
        let directory = self.walk(root, 1, parent).await?;
        let result = self
            .transact(
                P9_TLCREATE,
                |body| {
                    push_u32(body, directory);
                    push_string(body, name);
                    push_u32(body, P9_DOTL_WRONLY);
                    push_u32(body, 0o644);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(directory).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn truncate_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        let result = self.open(file, P9_DOTL_WRONLY | P9_DOTL_TRUNC).await;
        let _ = self.clunk(file).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn write_file_impl(
        &self,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        self.open(file, P9_DOTL_WRONLY).await?;
        let result = self.write_chunks(file, offset, bytes).await;
        let _ = self.clunk(file).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn remove_impl(&self, path: &str, directory: bool) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let root = self.attach_root().await?;
        let dir = self.walk(root, 1, parent).await?;
        let flags = if directory { P9_DOTL_AT_REMOVEDIR } else { 0 };
        let result = self
            .transact(
                P9_TUNLINKAT,
                |body| {
                    push_u32(body, dir);
                    push_string(body, name);
                    push_u32(body, flags);
                },
                16,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(dir).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn rename_impl(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (source_parent, source_name) = split_parent_name(source)?;
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let root = self.attach_root().await?;
        let source_dir = self.walk(root, 1, source_parent).await?;
        let destination_dir = self.walk(root, 2, destination_parent).await?;
        let result = self
            .transact(
                P9_TRENAMEAT,
                |body| {
                    push_u32(body, source_dir);
                    push_string(body, source_name);
                    push_u32(body, destination_dir);
                    push_string(body, destination_name);
                },
                16,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(destination_dir).await;
        let _ = self.clunk(source_dir).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn hard_link_impl(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let root = self.attach_root().await?;
        let source_fid = self.walk(root, 1, source).await?;
        let destination_dir = self.walk(root, 2, destination_parent).await?;
        let result = self
            .transact(
                P9_TLINK,
                |body| {
                    push_u32(body, destination_dir);
                    push_u32(body, source_fid);
                    push_string(body, destination_name);
                },
                64,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(destination_dir).await;
        let _ = self.clunk(source_fid).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn symlink_impl(&self, target: &str, link_path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(link_path)?;
        let root = self.attach_root().await?;
        let directory = self.walk(root, 1, parent).await?;
        let result = self
            .transact(
                P9_TSYMLINK,
                |body| {
                    push_u32(body, directory);
                    push_string(body, name);
                    push_string(body, target);
                    push_u32(body, P9_NOUID);
                },
                64,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(directory).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn read_link_impl(&self, path: &str) -> Result<String, HostFsError> {
        let root = self.attach_root().await?;
        let target = self.walk(root, 1, path).await?;
        let result = self
            .transact(P9_TREADLINK, |body| push_u32(body, target), 4096)
            .await
            .and_then(|response| {
                let mut cursor = 7;
                read_string(&response, &mut cursor)
            });
        let _ = self.clunk(target).await;
        let _ = self.clunk(root).await;
        result
    }
}

impl<Transport: HostFsTransport> HostFileSystem for HostFsClient<Transport> {
    fn stat_path<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<HostMetadata, HostFsError>> + Send + 'a {
        async move { self.stat_path_impl(path).await }
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + Send + 'a {
        async move { self.read_dir_impl(path).await }
    }

    fn read_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<Vec<u8>, HostFsError>> + Send + 'a {
        async move { self.read_file_impl(path).await }
    }

    fn read_file_range<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        max_bytes: u32,
    ) -> impl Future<Output = Result<Vec<u8>, HostFsError>> + Send + 'a {
        async move { self.read_file_range_impl(path, offset, max_bytes).await }
    }

    fn write_file<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.write_file_impl(path, offset, bytes).await }
    }

    fn truncate_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.truncate_file_impl(path).await }
    }

    fn create_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.create_file_impl(path).await }
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.create_directory_impl(path).await }
    }

    fn remove<'a>(
        &'a self,
        path: &'a str,
        directory: bool,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.remove_impl(path, directory).await }
    }

    fn rename<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.rename_impl(source, destination).await }
    }

    fn hard_link<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.hard_link_impl(source, destination).await }
    }

    fn symlink<'a>(
        &'a self,
        target: &'a str,
        link_path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.symlink_impl(target, link_path).await }
    }

    fn read_link<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<String, HostFsError>> + Send + 'a {
        async move { self.read_link_impl(path).await }
    }
}

fn split_path(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn split_parent_name(path: &str) -> Result<(&str, &str), HostFsError> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err(HostFsError::Transport(IoError::PermissionDenied));
    }
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some(parts) => parts,
        None => ("/", trimmed),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(HostFsError::Transport(IoError::PermissionDenied));
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent, name))
}

fn push_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_string(buf: &mut Vec<u8>, value: &str) {
    let len = u16::try_from(value.len()).expect("9p string field exceeded u16::MAX");
    push_u16(buf, len);
    buf.extend_from_slice(value.as_bytes());
}

fn read_u8(buf: &[u8], offset: usize) -> Result<u8, HostFsError> {
    buf.get(offset)
        .copied()
        .ok_or(HostFsError::Protocol("buffer underrun"))
}

fn read_u16_le(buf: &[u8], offset: usize) -> Result<u16, HostFsError> {
    let bytes = read_slice(buf, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(buf: &[u8], offset: usize) -> Result<u32, HostFsError> {
    let bytes = read_slice(buf, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64_le(buf: &[u8], offset: usize) -> Result<u64, HostFsError> {
    let bytes = read_slice(buf, offset, 8)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_slice(buf: &[u8], offset: usize, len: usize) -> Result<&[u8], HostFsError> {
    buf.get(offset..offset + len)
        .ok_or(HostFsError::Protocol("buffer underrun"))
}

fn read_string(buf: &[u8], cursor: &mut usize) -> Result<String, HostFsError> {
    let len = usize::from(read_u16_le(buf, *cursor)?);
    *cursor += 2;
    let bytes = read_slice(buf, *cursor, len)?;
    *cursor += len;
    String::from_utf8(bytes.to_vec()).map_err(|_| HostFsError::Utf8)
}
