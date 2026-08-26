extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use bytes::BytesMut;
use core::future::Future;
use helios_hal::io::IoError;
use objectpool::{Pool, ReusableObject};

use crate::{
    AuthorityDomain, HostDirEntry, HostFileSystem, HostFsError, HostMetadata, ObjectIdentity,
};

const DEFAULT_MSIZE: u32 = (1024 * 1024) + 24;
/// Number of pre-allocated 9p buffers retained by each client.
/// Sized so that the transport pipeline depth (currently single-flight)
/// plus a couple of in-flight retries always have a hot buffer waiting.
const P9_BUFFER_POOL_SLOTS: usize = 4;
/// Maximum capacity a returning request buffer is allowed to retain
/// before it gets re-allocated. Caps memory pressure when a single
/// large 9p TWRITE temporarily inflates a buffer.
const P9_BUFFER_RETAINED_CAPACITY: usize = (DEFAULT_MSIZE as usize).next_power_of_two();
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
const P9_TSETATTR: u8 = 26;
const P9_TREADDIR: u8 = 40;
const P9_TFSYNC: u8 = 50;
const P9_TLINK: u8 = 70;
const P9_TMKDIR: u8 = 72;
const P9_TRENAMEAT: u8 = 74;
const P9_TUNLINKAT: u8 = 76;
const P9_RLERROR: u8 = 7;
const P9_DOTL_RDONLY: u32 = 0;
const P9_DOTL_WRONLY: u32 = 1;
const P9_DOTL_DIRECTORY: u32 = 0o200000;
const P9_DOTL_AT_REMOVEDIR: u32 = 0x200;
const P9_STATS_BASIC: u64 = 0x0000_07ff;
const P9_SETATTR_SIZE: u32 = 0x0000_0008;
const P9_SETATTR_ATIME_SET: u32 = 0x0000_0080;
const P9_SETATTR_MTIME_SET: u32 = 0x0000_0100;
const P9_QTDIR: u8 = 0x80;
const P9_WRITE_CHUNK: usize = (DEFAULT_MSIZE as usize) - 24;
const P9_HEADER_LEN: usize = 7;
/// Byte offsets of the `Rgetattr` fields, measured from the start of the
/// message (the 7-byte `size[4] type[1] tag[2]` header included). 9P2000.L
/// fixes this layout, so the reply is parsed by offset instead of by a
/// running cursor that hides which fields are being skipped.
const P9_RGETATTR_QID_TYPE: usize = 15;
const P9_RGETATTR_QID_PATH: usize = 20;
const P9_RGETATTR_MODE: usize = 28;
const P9_RGETATTR_NLINK: usize = 40;
const P9_RGETATTR_SIZE: usize = 56;
const P9_RGETATTR_ATIME_SECONDS: usize = 80;
const P9_RGETATTR_MTIME_SECONDS: usize = 96;
const P9_RGETATTR_CTIME_SECONDS: usize = 112;
/// Full `Rgetattr` reply length, up to and including `data_version`.
const P9_RGETATTR_LEN: usize = 160;
const P9_DEFAULT_REQUEST_BODY_BYTES: usize = 256;
const P9_TWRITE_FIXED_BODY_BYTES: usize = 4 + 8 + 4;

pub trait HostFsTransport: Clone + Send + Sync + 'static {
    fn mount_tag(&self) -> &str;

    /// Submits a fully-formed 9p Tmessage and resolves with the
    /// reply. The request buffer is borrowed for the duration of the
    /// future so that callers may pool and recycle it.
    fn request<'a>(
        &'a self,
        bytes: &'a [u8],
        response: &'a mut BytesMut,
        response_len: usize,
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;
}

pub struct HostFsClient<Transport: HostFsTransport> {
    transport: Transport,
    request_buffers: Pool<BytesMut>,
    response_buffers: Pool<BytesMut>,
}

impl<Transport: HostFsTransport> Clone for HostFsClient<Transport> {
    fn clone(&self) -> Self {
        Self::new(self.transport.clone())
    }
}

impl<Transport: HostFsTransport> HostFsClient<Transport> {
    pub fn new(transport: Transport) -> Self {
        Self {
            transport,
            request_buffers: Pool::bounded(P9_BUFFER_POOL_SLOTS, BytesMut::new, reset_p9_buffer),
            response_buffers: Pool::bounded(P9_BUFFER_POOL_SLOTS, BytesMut::new, reset_p9_buffer),
        }
    }

    async fn transact(
        &self,
        ty: u8,
        body: impl FnOnce(&mut BytesMut),
        response_len: usize,
    ) -> Result<ReusableObject<BytesMut>, HostFsError> {
        self.transact_with_capacity(ty, P9_DEFAULT_REQUEST_BODY_BYTES, body, response_len)
            .await
    }

    async fn transact_with_capacity(
        &self,
        ty: u8,
        body_capacity: usize,
        body: impl FnOnce(&mut BytesMut),
        response_len: usize,
    ) -> Result<ReusableObject<BytesMut>, HostFsError> {
        let mut request = self.request_buffers.get_owned();
        let mut response = self.response_buffers.get_owned();
        request.clear();
        request.reserve(P9_HEADER_LEN + body_capacity);
        request.extend_from_slice(&0_u32.to_le_bytes());
        request.extend_from_slice(&[ty]);
        request.extend_from_slice(&P9_NOTAG.to_le_bytes());
        body(&mut request);
        let size =
            u32::try_from(request.len()).map_err(|_| HostFsError::Protocol("request too large"))?;
        request[..4].copy_from_slice(&size.to_le_bytes());

        self.transport
            .request(&request, &mut response, response_len)
            .await
            .map_err(HostFsError::Transport)?;
        drop(request);
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
        let segment_count = path_segment_count(path);
        let response = self
            .transact(
                P9_TWALK,
                |body| {
                    push_u32(body, parent_fid);
                    push_u32(body, new_fid);
                    push_u16(
                        body,
                        u16::try_from(segment_count).expect("walk segment count overflowed u16"),
                    );
                    for segment in path_segments(path) {
                        push_string(body, segment);
                    }
                },
                4096,
            )
            .await?;
        let cursor = 7;
        let walked = read_u16_le(&response, cursor)?;
        if usize::from(walked) != segment_count {
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
                P9_RGETATTR_LEN,
            )
            .await?;
        parse_getattr_reply(&response)
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

    async fn read_chunk_into(
        &self,
        fid: u32,
        offset: u64,
        max_bytes: u32,
        output: &mut Vec<u8>,
    ) -> Result<usize, HostFsError> {
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
        append_read_payload(&response, output)
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
            let read = self
                .read_chunk_into(fid, offset, request_count, &mut bytes)
                .await?;
            if read == 0 {
                return Err(HostFsError::Protocol("short 9p read"));
            }
            offset = offset
                .checked_add(u64::try_from(read).expect("chunk length overflowed u64"))
                .ok_or(HostFsError::Protocol("read offset overflowed u64"))?;
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
                .transact_with_capacity(
                    P9_TWRITE,
                    write_request_body_capacity(count),
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
        self.set_file_size_impl(path, 0).await
    }

    async fn set_file_size_impl(&self, path: &str, size: u64) -> Result<(), HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        let result = self
            .transact(
                P9_TSETATTR,
                |body| {
                    push_u32(body, file);
                    push_u32(body, P9_SETATTR_SIZE);
                    push_u32(body, 0);
                    push_u32(body, P9_NOUID);
                    push_u32(body, P9_NOUID);
                    push_u64(body, size);
                    push_u64(body, 0);
                    push_u64(body, 0);
                    push_u64(body, 0);
                    push_u64(body, 0);
                },
                7,
            )
            .await
            .map(|_| ());
        let _ = self.clunk(file).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn set_times_impl(
        &self,
        path: &str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
    ) -> Result<(), HostFsError> {
        let (
            valid,
            access_seconds,
            access_subnanoseconds,
            modified_seconds,
            modified_subnanoseconds,
        ) = p9_setattr_times(access_nanos, modified_nanos);
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        let result = self
            .transact(
                P9_TSETATTR,
                |body| {
                    push_u32(body, file);
                    push_u32(body, valid);
                    push_u32(body, 0);
                    push_u32(body, P9_NOUID);
                    push_u32(body, P9_NOUID);
                    push_u64(body, 0);
                    push_u64(body, access_seconds);
                    push_u64(body, access_subnanoseconds);
                    push_u64(body, modified_seconds);
                    push_u64(body, modified_subnanoseconds);
                },
                7,
            )
            .await
            .map(|_| ());
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

    /// Appends at the host's current end of file.
    ///
    /// 9p `Twrite` always carries an explicit offset — the server `pwrite`s,
    /// so `O_APPEND` on `Tlopen` would not make the write positionless. The
    /// end of file is therefore resolved with `Tgetattr` on the same fid that
    /// then performs the write, which is the closest the protocol gets to
    /// append semantics.
    async fn append_file_impl(&self, path: &str, bytes: &[u8]) -> Result<u64, HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        let result = async {
            let offset = self.get_attr(file).await?.size;
            self.open(file, P9_DOTL_WRONLY).await?;
            self.write_chunks(file, offset, bytes).await?;
            Ok(offset)
        }
        .await;
        let _ = self.clunk(file).await;
        let _ = self.clunk(root).await;
        result
    }

    async fn sync_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        let root = self.attach_root().await?;
        let file = self.walk(root, 1, path).await?;
        let result = async {
            // `Tfsync` requires an open fid; the host flushes the underlying
            // file description, so a read-only open is enough to name it.
            self.open(file, P9_DOTL_RDONLY).await?;
            self.transact(
                P9_TFSYNC,
                |body| {
                    push_u32(body, file);
                    push_u32(body, 0);
                },
                P9_HEADER_LEN,
            )
            .await
            .map(|_| ())
        }
        .await;
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

    fn append_file<'a>(
        &'a self,
        path: &'a str,
        bytes: &'a [u8],
    ) -> impl Future<Output = Result<u64, HostFsError>> + Send + 'a {
        async move { self.append_file_impl(path, bytes).await }
    }

    fn sync_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.sync_file_impl(path).await }
    }

    fn truncate_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.truncate_file_impl(path).await }
    }

    fn set_file_size<'a>(
        &'a self,
        path: &'a str,
        size: u64,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move { self.set_file_size_impl(path, size).await }
    }

    fn set_times<'a>(
        &'a self,
        path: &'a str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a {
        async move {
            self.set_times_impl(path, access_nanos, modified_nanos)
                .await
        }
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

fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|segment| !segment.is_empty())
}

fn path_segment_count(path: &str) -> usize {
    path_segments(path).count()
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

fn p9_setattr_times(
    access_nanos: Option<u64>,
    modified_nanos: Option<u64>,
) -> (u32, u64, u64, u64, u64) {
    let (access_valid, access_seconds, access_subnanoseconds) =
        p9_optional_timestamp(P9_SETATTR_ATIME_SET, access_nanos);
    let (modified_valid, modified_seconds, modified_subnanoseconds) =
        p9_optional_timestamp(P9_SETATTR_MTIME_SET, modified_nanos);
    (
        access_valid | modified_valid,
        access_seconds,
        access_subnanoseconds,
        modified_seconds,
        modified_subnanoseconds,
    )
}

fn p9_optional_timestamp(flag: u32, nanos: Option<u64>) -> (u32, u64, u64) {
    let Some(nanos) = nanos else {
        return (0, 0, 0);
    };
    (flag, nanos / 1_000_000_000, nanos % 1_000_000_000)
}

fn write_request_body_capacity(payload_len: usize) -> usize {
    P9_TWRITE_FIXED_BODY_BYTES
        .checked_add(payload_len)
        .expect("9p write request capacity overflowed")
}

fn reset_p9_buffer(buf: &mut BytesMut) {
    buf.clear();
    if buf.capacity() > P9_BUFFER_RETAINED_CAPACITY {
        // Shrink any one-shot oversize back so the pool does not
        // permanently hold inflated buffers from a transient large
        // 9p TWRITE.
        *buf = BytesMut::with_capacity(P9_BUFFER_RETAINED_CAPACITY);
    }
}

fn push_u16(buf: &mut BytesMut, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buf: &mut BytesMut, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(buf: &mut BytesMut, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_string(buf: &mut BytesMut, value: &str) {
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

/// Decodes an `Rgetattr` reply into the kernel's host metadata type.
///
/// `st_dev`/`st_ino` identity comes from the qid path within the host-share
/// authority domain, which is stable for the lifetime of a mount; timestamps
/// are the host's own, normalised to nanoseconds since the Unix epoch.
fn parse_getattr_reply(response: &[u8]) -> Result<HostMetadata, HostFsError> {
    let qid_type = read_u8(response, P9_RGETATTR_QID_TYPE)?;
    let qid_path = read_u64_le(response, P9_RGETATTR_QID_PATH)?;
    let mode = read_u32_le(response, P9_RGETATTR_MODE)?;
    let link_count = read_u64_le(response, P9_RGETATTR_NLINK)?;
    let size = read_u64_le(response, P9_RGETATTR_SIZE)?;
    let access_nanos = read_p9_timestamp(response, P9_RGETATTR_ATIME_SECONDS)?;
    let modified_nanos = read_p9_timestamp(response, P9_RGETATTR_MTIME_SECONDS)?;
    let status_nanos = read_p9_timestamp(response, P9_RGETATTR_CTIME_SECONDS)?;
    Ok(HostMetadata {
        identity: ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, qid_path),
        qid_path,
        qid_type,
        mode,
        size,
        link_count,
        access_nanos,
        modified_nanos,
        status_nanos,
    })
}

/// Reads a `seconds[8] nanoseconds[8]` 9p timestamp pair as nanoseconds.
fn read_p9_timestamp(buf: &[u8], offset: usize) -> Result<u64, HostFsError> {
    let seconds = read_u64_le(buf, offset)?;
    let subnanoseconds = read_u64_le(buf, offset + 8)?;
    Ok(seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(subnanoseconds))
}

fn append_read_payload(response: &[u8], output: &mut Vec<u8>) -> Result<usize, HostFsError> {
    let mut cursor = 7;
    let count = usize::try_from(read_u32_le(response, cursor)?)
        .map_err(|_| HostFsError::Protocol("read payload count overflowed usize"))?;
    cursor += 4;
    output.extend_from_slice(read_slice(response, cursor, count)?);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p9_setattr_times_uses_specific_timestamp_flags() {
        assert_eq!(
            p9_setattr_times(Some(1_500_000_002), None),
            (P9_SETATTR_ATIME_SET, 1, 500_000_002, 0, 0)
        );
        assert_eq!(
            p9_setattr_times(None, Some(2_000_000_003)),
            (P9_SETATTR_MTIME_SET, 0, 0, 2, 3)
        );
        assert_eq!(
            p9_setattr_times(Some(4), Some(5)),
            (P9_SETATTR_ATIME_SET | P9_SETATTR_MTIME_SET, 0, 4, 0, 5)
        );
    }

    #[test]
    fn path_segments_skip_empty_components_without_allocating() {
        let segments = path_segments("/alpha//beta/gamma/").collect::<alloc::vec::Vec<_>>();

        assert_eq!(segments, ["alpha", "beta", "gamma"]);
        assert_eq!(path_segment_count("/alpha//beta/gamma/"), 3);
    }

    #[test]
    fn append_read_payload_extends_existing_buffer() {
        let mut response = Vec::new();
        response.extend_from_slice(&14u32.to_le_bytes());
        response.push(P9_TREAD + 1);
        response.extend_from_slice(&P9_NOTAG.to_le_bytes());
        response.extend_from_slice(&3u32.to_le_bytes());
        response.extend_from_slice(b"abc");

        let mut output = Vec::from(b"prefix-".as_slice());
        let read = append_read_payload(&response, &mut output)
            .expect("read response payload should append");

        assert_eq!(read, 3);
        assert_eq!(output, b"prefix-abc");
    }

    /// Builds an `Rgetattr` reply with the 9P2000.L field layout.
    fn rgetattr_reply(
        qid_type: u8,
        qid_path: u64,
        mode: u32,
        link_count: u64,
        size: u64,
        access: (u64, u64),
        modified: (u64, u64),
        status: (u64, u64),
    ) -> alloc::vec::Vec<u8> {
        let mut reply = alloc::vec![0_u8; P9_RGETATTR_LEN];
        reply[..4].copy_from_slice(&(P9_RGETATTR_LEN as u32).to_le_bytes());
        reply[4] = P9_TGETATTR + 1;
        reply[5..7].copy_from_slice(&P9_NOTAG.to_le_bytes());
        reply[7..15].copy_from_slice(&P9_STATS_BASIC.to_le_bytes());
        reply[P9_RGETATTR_QID_TYPE] = qid_type;
        reply[P9_RGETATTR_QID_PATH..P9_RGETATTR_QID_PATH + 8]
            .copy_from_slice(&qid_path.to_le_bytes());
        reply[P9_RGETATTR_MODE..P9_RGETATTR_MODE + 4].copy_from_slice(&mode.to_le_bytes());
        reply[P9_RGETATTR_NLINK..P9_RGETATTR_NLINK + 8]
            .copy_from_slice(&link_count.to_le_bytes());
        reply[P9_RGETATTR_SIZE..P9_RGETATTR_SIZE + 8].copy_from_slice(&size.to_le_bytes());
        for (offset, (seconds, subnanoseconds)) in [
            (P9_RGETATTR_ATIME_SECONDS, access),
            (P9_RGETATTR_MTIME_SECONDS, modified),
            (P9_RGETATTR_CTIME_SECONDS, status),
        ] {
            reply[offset..offset + 8].copy_from_slice(&seconds.to_le_bytes());
            reply[offset + 8..offset + 16].copy_from_slice(&subnanoseconds.to_le_bytes());
        }
        reply
    }

    #[test]
    fn getattr_reply_carries_identity_link_count_and_timestamps() {
        let reply = rgetattr_reply(
            0,
            0x1234_5678_9abc_def0,
            0o100_644,
            3,
            4096,
            (11, 12),
            (13, 14),
            (15, 16),
        );

        let metadata = parse_getattr_reply(&reply).expect("Rgetattr reply should decode");

        assert_eq!(metadata.identity.domain(), AuthorityDomain::HOST_SHARE_9P);
        assert_eq!(metadata.identity.local(), 0x1234_5678_9abc_def0);
        assert_eq!(metadata.qid_path, 0x1234_5678_9abc_def0);
        assert_eq!(metadata.mode, 0o100_644);
        assert_eq!(metadata.link_count, 3);
        assert_eq!(metadata.size, 4096);
        assert_eq!(metadata.access_nanos, 11_000_000_012);
        assert_eq!(metadata.modified_nanos, 13_000_000_014);
        assert_eq!(metadata.status_nanos, 15_000_000_016);
    }

    #[test]
    fn getattr_reply_reports_directory_qid_type() {
        let reply = rgetattr_reply(P9_QTDIR, 7, 0o040_755, 2, 0, (0, 0), (0, 0), (0, 0));

        let metadata = parse_getattr_reply(&reply).expect("Rgetattr reply should decode");

        assert_eq!(metadata.qid_type & P9_QTDIR, P9_QTDIR);
        assert_eq!(metadata.identity.local(), 7);
    }

    #[test]
    fn getattr_reply_shorter_than_the_fixed_layout_is_rejected() {
        let mut reply = rgetattr_reply(0, 1, 0, 1, 0, (0, 0), (0, 0), (0, 0));
        reply.truncate(P9_RGETATTR_CTIME_SECONDS + 4);

        assert!(matches!(
            parse_getattr_reply(&reply),
            Err(HostFsError::Protocol(_))
        ));
    }

    #[test]
    fn write_request_capacity_accounts_for_payload() {
        assert_eq!(
            write_request_body_capacity(4096),
            P9_TWRITE_FIXED_BODY_BYTES + 4096
        );
    }
}
