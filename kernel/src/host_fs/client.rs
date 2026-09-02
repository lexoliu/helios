//! In-kernel 9p client for the virtio host share.
//!
//! # Concurrency contract
//!
//! One client serves every task that touches the host share, and
//! cloning it shares the session, the handle allocators and the buffer
//! pools rather than opening a second conversation with the server. N
//! operations therefore run concurrently over one 9p session:
//!
//! * The session — the msize the server granted and the root fid it
//!   attached — is established once, lazily, by whichever task needs it
//!   first. The async mutex that serialises establishment is held
//!   across the handshake because that is its entire job; every later
//!   reader takes no lock at all.
//! * Each operation owns its tags and fids: a tag for the lifetime of
//!   one message, a fid for the lifetime of the operation, both handed
//!   out by allocators behind a spin mutex that is taken only to
//!   allocate or release and never held across an await. Fids are
//!   returned on every exit path, error paths included, and the
//!   session's own root fid is never opened, walked over or clunked by
//!   an operation — every operation walks a fid of its own from it.
//! * A reply is matched against the tag and the message type of the
//!   request it answers. A mismatch is a protocol error, not something
//!   to parse anyway.
//! * The transport bounds how many requests are in flight at once; the
//!   buffer pools are sized to that same depth, so the pipeline is
//!   never waiting on a buffer a finished request has not yet returned.
//!
//! Every size that depends on the server — read and write chunking,
//! readdir batches, response buffer sizing — is derived from the
//! negotiated msize rather than from the size this client asked for.

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use bytes::BytesMut;
use core::future::Future;
use core::sync::atomic::{AtomicU64, Ordering};
use helios_hal::io::IoError;
use objectpool::{Pool, ReusableObject};
use slab::Slab;
use spin::Mutex as SpinMutex;
use triomphe::Arc;

use crate::{
    AuthorityDomain, HostDirEntry, HostFileSystem, HostFsError, HostMetadata, ObjectIdentity,
    RawMutex,
};

/// The msize this client asks for. The server answers with its own
/// bound, which is what every later size is derived from.
const P9_REQUESTED_MSIZE: u32 = (1024 * 1024) + 24;
/// The smallest msize this client will work with. A server that offers
/// less cannot carry a directory entry batch or a `Twalk` reply in one
/// message, so the mount is rejected instead of limping.
const P9_MIN_MSIZE: u32 = 8192;
/// Longest fixed prefix any message puts in front of its payload:
/// `Twrite`'s `size[4] type[1] tag[2] fid[4] offset[8] count[4]`.
/// Subtracting it from the msize gives the payload a message may carry
/// in either direction.
const P9_MAX_FIXED_HEADER_BYTES: u32 = 23;
/// Maximum capacity a returning request buffer is allowed to retain
/// before it gets re-allocated. Caps memory pressure when a single
/// large 9p TWRITE temporarily inflates a buffer.
const P9_BUFFER_RETAINED_CAPACITY: usize = (P9_REQUESTED_MSIZE as usize).next_power_of_two();
const P9_VERSION: &str = "9P2000.L";
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
/// Reply sizes that the protocol fixes independently of the msize.
/// Every one of them has to leave room for an `Rlerror`, which is
/// 11 bytes: a reply the buffer cannot hold arrives truncated and is
/// reported as a transport fault instead of the error the server sent.
const P9_RVERSION_LEN: usize = 64;
const P9_RWALK_LEN: usize = 4096;
const P9_RLINK_LEN: usize = 4096;
const P9_SMALL_REPLY_LEN: usize = 64;

pub trait HostFsTransport: Clone + Send + Sync + 'static {
    fn mount_tag(&self) -> &str;

    /// How many requests this transport carries at once before a
    /// submission has to wait for a completion. Clients size their
    /// pipeline, and the buffers that feed it, from this.
    fn pipeline_depth(&self) -> usize;

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

/// An established 9p session: what the server granted and the fid every
/// operation walks from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Session {
    msize: u32,
    root_fid: u32,
}

impl Session {
    /// The largest payload one message may carry in either direction.
    fn payload_chunk(self) -> usize {
        usize::try_from(self.msize - P9_MAX_FIXED_HEADER_BYTES)
            .expect("a 9p payload chunk fits in usize")
    }

    /// Response buffer size for a reply that may fill a whole message.
    fn message_len(self) -> usize {
        usize::try_from(self.msize).expect("a 9p msize fits in usize")
    }
}

/// Publishes the session exactly once.
///
/// Readers take no lock: a session is two `u32`s, and a granted msize
/// is never zero, so both halves publish together through a single
/// atomic word whose zero value means "not established yet". Writers
/// serialise on `establishing` so exactly one `Tversion`/`Tattach` pair
/// ever runs, and a handshake that fails leaves the cell unpublished
/// for the next caller to retry.
struct SessionCell {
    published: AtomicU64,
    establishing: RawMutex,
}

impl SessionCell {
    fn new() -> Self {
        Self {
            published: AtomicU64::new(0),
            establishing: RawMutex::new(),
        }
    }

    fn get(&self) -> Option<Session> {
        let word = self.published.load(Ordering::Acquire);
        let msize = (word >> 32) as u32;
        (msize != 0).then_some(Session {
            msize,
            root_fid: word as u32,
        })
    }

    fn publish(&self, session: Session) {
        let word = (u64::from(session.msize) << 32) | u64::from(session.root_fid);
        self.published.store(word, Ordering::Release);
    }
}

/// A dense allocator for one 9p handle space.
///
/// Tags and fids need the same thing from their space: a value that is
/// unique among everything currently outstanding and never the space's
/// reserved sentinel (`NOTAG`, `NOFID`). A slab hands out the lowest
/// free index, so a handle is bounded by how many are live at once and
/// the sentinel is only ever reached by a caller that leaked handles —
/// which is a fault to report, not a condition to recover from.
struct HandleAllocator {
    live: SpinMutex<Slab<()>>,
    /// The value this space reserves — `NOTAG` or `NOFID` — and
    /// therefore the exclusive upper bound on a handle.
    reserved: u32,
    space: &'static str,
    exhausted: &'static str,
}

impl HandleAllocator {
    fn new(capacity: usize, reserved: u32, space: &'static str, exhausted: &'static str) -> Self {
        Self {
            live: SpinMutex::new(Slab::with_capacity(capacity)),
            reserved,
            space,
            exhausted,
        }
    }

    fn allocate(&self) -> Result<Handle<'_>, HostFsError> {
        let value = {
            let mut live = self.live.lock();
            let key = live.insert(());
            match u32::try_from(key) {
                Ok(value) if value < self.reserved => value,
                _ => {
                    live.remove(key);
                    return Err(HostFsError::Protocol(self.exhausted));
                }
            }
        };
        Ok(Handle {
            allocator: self,
            value,
        })
    }

    fn release(&self, value: u32) {
        let key = usize::try_from(value).expect("a 9p handle fits in usize");
        let mut live = self.live.lock();
        assert!(
            live.contains(key),
            "9p {} {value} was released twice",
            self.space
        );
        live.remove(key);
    }

    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.live.lock().len()
    }
}

/// A handle borrowed from one of the client's handle spaces.
///
/// Dropping it returns the number to its allocator. The server side of
/// a fid is released by `Tclunk`, which is async and therefore stays an
/// explicit step; the number itself is never leaked by an early return.
struct Handle<'a> {
    allocator: &'a HandleAllocator,
    value: u32,
}

impl Handle<'_> {
    fn value(&self) -> u32 {
        self.value
    }

    /// Keeps this handle allocated for the client's lifetime.
    ///
    /// Only the session root fid uses this: it is attached once and
    /// stays valid until the client goes away.
    fn retain(self) -> u32 {
        let value = self.value;
        core::mem::forget(self);
        value
    }
}

impl Drop for Handle<'_> {
    fn drop(&mut self) {
        self.allocator.release(self.value);
    }
}

/// State every clone of a client shares.
struct ClientInner<Transport: HostFsTransport> {
    transport: Transport,
    session: SessionCell,
    tags: HandleAllocator,
    fids: HandleAllocator,
    request_buffers: Pool<BytesMut>,
    response_buffers: Pool<BytesMut>,
}

pub struct HostFsClient<Transport: HostFsTransport> {
    inner: Arc<ClientInner<Transport>>,
}

impl<Transport: HostFsTransport> Clone for HostFsClient<Transport> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Transport: HostFsTransport> HostFsClient<Transport> {
    pub fn new(transport: Transport) -> Self {
        let depth = transport.pipeline_depth();
        assert!(
            depth != 0,
            "a host-fs transport must carry at least one request"
        );
        Self {
            inner: Arc::new(ClientInner {
                session: SessionCell::new(),
                tags: HandleAllocator::new(
                    depth,
                    u32::from(P9_NOTAG),
                    "tag",
                    "9p tag space exhausted",
                ),
                fids: HandleAllocator::new(depth, P9_NOFID, "fid", "9p fid space exhausted"),
                request_buffers: Pool::bounded(depth, BytesMut::new, reset_p9_buffer),
                response_buffers: Pool::bounded(depth, BytesMut::new, reset_p9_buffer),
                transport,
            }),
        }
    }

    /// The session, establishing it if this is the first use.
    async fn session(&self) -> Result<Session, HostFsError> {
        if let Some(session) = self.inner.session.get() {
            return Ok(session);
        }
        let _establishing = self.inner.session.establishing.lock().await;
        // Another task may have finished the handshake while this one
        // waited for the lock.
        if let Some(session) = self.inner.session.get() {
            return Ok(session);
        }
        let session = self.establish().await?;
        self.inner.session.publish(session);
        Ok(session)
    }

    /// Runs the `Tversion`/`Tattach` handshake that opens a session.
    async fn establish(&self) -> Result<Session, HostFsError> {
        let msize = self.version().await?;
        let root = self.inner.fids.allocate()?;
        let mount_tag = self.inner.transport.mount_tag().to_owned();
        let root_fid = root.value();
        self.transact(
            P9_TATTACH,
            move |body| {
                push_u32(body, root_fid);
                push_u32(body, P9_NOFID);
                push_string(body, "root");
                push_string(body, &mount_tag);
                push_u32(body, P9_NOUID);
            },
            P9_SMALL_REPLY_LEN,
        )
        .await?;
        tracing::info!(
            msize,
            root_fid,
            "9p session established with the host share"
        );
        Ok(Session {
            msize,
            root_fid: root.retain(),
        })
    }

    /// Negotiates the protocol version and returns the granted msize.
    ///
    /// `Tversion` is the one message that carries `NOTAG`: it resets the
    /// session, so no other request may be outstanding when it runs.
    async fn version(&self) -> Result<u32, HostFsError> {
        let response = self
            .exchange(
                P9_NOTAG,
                P9_TVERSION,
                P9_DEFAULT_REQUEST_BODY_BYTES,
                |body| {
                    push_u32(body, P9_REQUESTED_MSIZE);
                    push_string(body, P9_VERSION);
                },
                P9_RVERSION_LEN,
            )
            .await?;
        let msize = read_u32_le(&response, P9_HEADER_LEN)?;
        let mut cursor = P9_HEADER_LEN + 4;
        let version = read_string(&response, &mut cursor)?;
        if version != P9_VERSION {
            tracing::error!(%version, "the host share answered with another 9p dialect");
            return Err(HostFsError::Protocol("server does not speak 9P2000.L"));
        }
        if msize > P9_REQUESTED_MSIZE {
            return Err(HostFsError::Protocol(
                "server granted a larger msize than the client asked for",
            ));
        }
        if msize < P9_MIN_MSIZE {
            return Err(HostFsError::Protocol("server msize is too small to use"));
        }
        Ok(msize)
    }

    /// Sends one message under a freshly allocated tag.
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
        let tag = self.inner.tags.allocate()?;
        let value = u16::try_from(tag.value()).expect("a 9p tag is bounded below NOTAG");
        self.exchange(value, ty, body_capacity, body, response_len)
            .await
    }

    /// Encodes one Tmessage, hands it to the transport and validates the
    /// reply against the request that produced it.
    async fn exchange(
        &self,
        tag: u16,
        ty: u8,
        body_capacity: usize,
        body: impl FnOnce(&mut BytesMut),
        response_len: usize,
    ) -> Result<ReusableObject<BytesMut>, HostFsError> {
        let mut request = self.inner.request_buffers.get_owned();
        let mut response = self.inner.response_buffers.get_owned();
        request.clear();
        request.reserve(P9_HEADER_LEN + body_capacity);
        request.extend_from_slice(&0_u32.to_le_bytes());
        request.extend_from_slice(&[ty]);
        request.extend_from_slice(&tag.to_le_bytes());
        body(&mut request);
        let size =
            u32::try_from(request.len()).map_err(|_| HostFsError::Protocol("request too large"))?;
        request[..4].copy_from_slice(&size.to_le_bytes());

        self.inner
            .transport
            .request(&request, &mut response, response_len)
            .await
            .map_err(HostFsError::Transport)?;
        drop(request);
        if response.len() < P9_HEADER_LEN {
            return Err(HostFsError::Protocol("response shorter than 9p header"));
        }
        let actual_size = read_u32_le(&response, 0)? as usize;
        if actual_size != response.len() {
            return Err(HostFsError::Protocol("response length header mismatch"));
        }

        // The reply has to name the request it answers before anything
        // in it is worth reading: several requests are in flight at
        // once, and a transport that crossed two of them must not be
        // parsed as if it had not.
        let response_tag = read_u16_le(&response, 5)?;
        if response_tag != tag {
            tracing::error!(
                expected = tag,
                observed = response_tag,
                "a 9p reply named another request's tag"
            );
            return Err(HostFsError::Protocol("9p reply tag did not match"));
        }

        let response_ty = response[4];
        if response_ty == P9_RLERROR {
            return Err(HostFsError::Server(read_u32_le(&response, P9_HEADER_LEN)?));
        }

        let expected = ty
            .checked_add(1)
            .ok_or(HostFsError::Protocol("response type overflowed"))?;
        if response_ty != expected {
            return Err(HostFsError::Protocol("unexpected 9p response type"));
        }

        Ok(response)
    }

    /// Walks `path` from the session root into a fid of its own.
    ///
    /// A zero-element walk clones the root fid, which is what makes "/"
    /// an ordinary path here: the caller always receives a fid it owns
    /// and clunks, and the session's root fid is never opened or
    /// clunked by an operation.
    async fn walk(&self, session: Session, path: &str) -> Result<Handle<'_>, HostFsError> {
        let target = self.inner.fids.allocate()?;
        let segment_count = path_segment_count(path);
        let parent_fid = session.root_fid;
        let new_fid = target.value();
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
                P9_RWALK_LEN,
            )
            .await?;
        let walked = read_u16_le(&response, P9_HEADER_LEN)?;
        if usize::from(walked) != segment_count {
            // The server walked part of the path; the remaining segment
            // doesn't exist. 9p leaves `new_fid` unattached in this case
            // per the 2000.L spec ("if any walked element doesn't exist
            // the final fid is not set"), so there is nothing to clunk —
            // dropping the handle is enough.
            return Err(HostFsError::Transport(IoError::NotFound));
        }
        Ok(target)
    }

    /// Releases a fid on both sides: `Tclunk` for the server, the
    /// handle's drop for the client's fid space.
    ///
    /// A failed clunk cannot be acted on — the operation it belonged to
    /// has already produced its result — so it is logged rather than
    /// returned.
    async fn clunk(&self, fid: Handle<'_>) {
        let value = fid.value();
        if let Err(error) = self
            .transact(P9_TCLUNK, |body| push_u32(body, value), P9_SMALL_REPLY_LEN)
            .await
        {
            tracing::warn!(fid = value, ?error, "9p clunk failed");
        }
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
        session: Session,
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
                session.message_len(),
            )
            .await?;
        let mut payload = Vec::new();
        append_read_payload(&response, &mut payload)?;
        Ok(payload)
    }

    async fn read_chunk_into(
        &self,
        session: Session,
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
                session.message_len(),
            )
            .await?;
        append_read_payload(&response, output)
    }

    async fn read_file_all(
        &self,
        session: Session,
        fid: u32,
        expected_size: u64,
    ) -> Result<Vec<u8>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY).await?;

        let chunk = session.payload_chunk();
        let expected_size = usize::try_from(expected_size)
            .map_err(|_| HostFsError::Protocol("file size overflowed usize"))?;
        let mut bytes = Vec::with_capacity(expected_size.min(chunk));
        let mut offset = 0_u64;
        while bytes.len() < expected_size {
            let remaining = expected_size - bytes.len();
            let request_count = remaining.min(chunk);
            let request_count =
                u32::try_from(request_count).expect("read request count overflowed u32");
            let read = self
                .read_chunk_into(session, fid, offset, request_count, &mut bytes)
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

    async fn read_dir_entries(
        &self,
        session: Session,
        fid: u32,
    ) -> Result<Vec<HostDirEntry>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY | P9_DOTL_DIRECTORY).await?;

        let batch = u32::try_from(session.payload_chunk()).expect("a readdir batch fits in u32");
        let mut entries = Vec::new();
        let mut offset = 0_u64;
        loop {
            let response = self
                .transact(
                    P9_TREADDIR,
                    |body| {
                        push_u32(body, fid);
                        push_u64(body, offset);
                        push_u32(body, batch);
                    },
                    session.message_len(),
                )
                .await?;
            let mut cursor = P9_HEADER_LEN;
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

    async fn open(&self, fid: u32, flags: u32) -> Result<(), HostFsError> {
        self.transact(
            P9_TLOPEN,
            |body| {
                push_u32(body, fid);
                push_u32(body, flags);
            },
            P9_SMALL_REPLY_LEN,
        )
        .await
        .map(|_| ())
    }

    async fn write_chunks(
        &self,
        session: Session,
        fid: u32,
        mut offset: u64,
        mut bytes: &[u8],
    ) -> Result<(), HostFsError> {
        let chunk = session.payload_chunk();
        while !bytes.is_empty() {
            let count = bytes.len().min(chunk);
            let payload = &bytes[..count];
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
                        body.extend_from_slice(payload);
                    },
                    P9_SMALL_REPLY_LEN,
                )
                .await?;
            let written = usize::try_from(read_u32_le(&response, P9_HEADER_LEN)?)
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
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let metadata = self.get_attr(target.value()).await;
        self.clunk(target).await;
        metadata
    }

    async fn read_dir_impl(&self, path: &str) -> Result<Vec<HostDirEntry>, HostFsError> {
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let entries = self.read_dir_entries(session, target.value()).await;
        self.clunk(target).await;
        entries
    }

    async fn read_file_impl(&self, path: &str) -> Result<Vec<u8>, HostFsError> {
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let data = async {
            let metadata = self.get_attr(target.value()).await?;
            self.read_file_all(session, target.value(), metadata.size)
                .await
        }
        .await;
        if let Err(error) = &data {
            tracing::warn!("host-fs read_file({path}) failed: {error:?}");
        }
        self.clunk(target).await;
        data
    }

    async fn read_file_range_impl(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, HostFsError> {
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let data = async {
            self.open(target.value(), P9_DOTL_RDONLY).await?;
            // The reply has to fit in one message, so a caller asking
            // for more than that gets a short read rather than a
            // request the server would have to truncate.
            let max_bytes = max_bytes
                .min(u32::try_from(session.payload_chunk()).expect("a payload chunk fits in u32"));
            self.read_chunk(session, target.value(), offset, max_bytes)
                .await
        }
        .await;
        if let Err(error) = &data {
            tracing::warn!(
                "host-fs read_file_range(path={path}, offset={offset}, max_bytes={max_bytes}) failed: {error:?}"
            );
        }
        self.clunk(target).await;
        data
    }

    async fn create_directory_impl(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let session = self.session().await?;
        let directory = self.walk(session, parent).await?;
        let fid = directory.value();
        let result = self
            .transact(
                P9_TMKDIR,
                |body| {
                    push_u32(body, fid);
                    push_string(body, name);
                    push_u32(body, 0o755);
                    push_u32(body, P9_NOUID);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(directory).await;
        result
    }

    async fn create_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let session = self.session().await?;
        let directory = self.walk(session, parent).await?;
        let fid = directory.value();
        let result = self
            .transact(
                P9_TLCREATE,
                |body| {
                    push_u32(body, fid);
                    push_string(body, name);
                    push_u32(body, P9_DOTL_WRONLY);
                    push_u32(body, 0o644);
                    push_u32(body, P9_NOUID);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(directory).await;
        result
    }

    async fn truncate_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        self.set_file_size_impl(path, 0).await
    }

    async fn set_file_size_impl(&self, path: &str, size: u64) -> Result<(), HostFsError> {
        let session = self.session().await?;
        let file = self.walk(session, path).await?;
        let fid = file.value();
        let result = self
            .transact(
                P9_TSETATTR,
                |body| {
                    push_u32(body, fid);
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
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(file).await;
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
        let session = self.session().await?;
        let file = self.walk(session, path).await?;
        let fid = file.value();
        let result = self
            .transact(
                P9_TSETATTR,
                |body| {
                    push_u32(body, fid);
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
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(file).await;
        result
    }

    async fn write_file_impl(
        &self,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), HostFsError> {
        let session = self.session().await?;
        let file = self.walk(session, path).await?;
        let result = async {
            self.open(file.value(), P9_DOTL_WRONLY).await?;
            self.write_chunks(session, file.value(), offset, bytes)
                .await
        }
        .await;
        self.clunk(file).await;
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
        let session = self.session().await?;
        let file = self.walk(session, path).await?;
        let result = async {
            let offset = self.get_attr(file.value()).await?.size;
            self.open(file.value(), P9_DOTL_WRONLY).await?;
            self.write_chunks(session, file.value(), offset, bytes)
                .await?;
            Ok(offset)
        }
        .await;
        self.clunk(file).await;
        result
    }

    async fn sync_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        let session = self.session().await?;
        let file = self.walk(session, path).await?;
        let fid = file.value();
        let result = async {
            // `Tfsync` requires an open fid; the host flushes the underlying
            // file description, so a read-only open is enough to name it.
            self.open(fid, P9_DOTL_RDONLY).await?;
            self.transact(
                P9_TFSYNC,
                |body| {
                    push_u32(body, fid);
                    push_u32(body, 0);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ())
        }
        .await;
        self.clunk(file).await;
        result
    }

    async fn remove_impl(&self, path: &str, directory: bool) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let session = self.session().await?;
        let dir = self.walk(session, parent).await?;
        let fid = dir.value();
        let flags = if directory { P9_DOTL_AT_REMOVEDIR } else { 0 };
        let result = self
            .transact(
                P9_TUNLINKAT,
                |body| {
                    push_u32(body, fid);
                    push_string(body, name);
                    push_u32(body, flags);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(dir).await;
        result
    }

    async fn rename_impl(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (source_parent, source_name) = split_parent_name(source)?;
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let session = self.session().await?;
        let source_dir = self.walk(session, source_parent).await?;
        let destination_dir = match self.walk(session, destination_parent).await {
            Ok(fid) => fid,
            Err(error) => {
                self.clunk(source_dir).await;
                return Err(error);
            }
        };
        let source_fid = source_dir.value();
        let destination_fid = destination_dir.value();
        let result = self
            .transact(
                P9_TRENAMEAT,
                |body| {
                    push_u32(body, source_fid);
                    push_string(body, source_name);
                    push_u32(body, destination_fid);
                    push_string(body, destination_name);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(destination_dir).await;
        self.clunk(source_dir).await;
        result
    }

    async fn hard_link_impl(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let session = self.session().await?;
        let source_file = self.walk(session, source).await?;
        let destination_dir = match self.walk(session, destination_parent).await {
            Ok(fid) => fid,
            Err(error) => {
                self.clunk(source_file).await;
                return Err(error);
            }
        };
        let source_fid = source_file.value();
        let destination_fid = destination_dir.value();
        let result = self
            .transact(
                P9_TLINK,
                |body| {
                    push_u32(body, destination_fid);
                    push_u32(body, source_fid);
                    push_string(body, destination_name);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(destination_dir).await;
        self.clunk(source_file).await;
        result
    }

    async fn symlink_impl(&self, target: &str, link_path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(link_path)?;
        let session = self.session().await?;
        let directory = self.walk(session, parent).await?;
        let fid = directory.value();
        let result = self
            .transact(
                P9_TSYMLINK,
                |body| {
                    push_u32(body, fid);
                    push_string(body, name);
                    push_string(body, target);
                    push_u32(body, P9_NOUID);
                },
                P9_SMALL_REPLY_LEN,
            )
            .await
            .map(|_| ());
        self.clunk(directory).await;
        result
    }

    async fn read_link_impl(&self, path: &str) -> Result<String, HostFsError> {
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let fid = target.value();
        let result = self
            .transact(P9_TREADLINK, |body| push_u32(body, fid), P9_RLINK_LEN)
            .await
            .and_then(|response| {
                let mut cursor = P9_HEADER_LEN;
                read_string(&response, &mut cursor)
            });
        self.clunk(target).await;
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
    use futures_lite::future::{block_on, zip};

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
        reply[P9_RGETATTR_NLINK..P9_RGETATTR_NLINK + 8].copy_from_slice(&link_count.to_le_bytes());
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

    /// The single file the fake server serves under any path.
    const FILE_CONTENT_LEN: usize = 40_000;

    /// What the fake server should do differently from the happy path.
    #[derive(Clone, Copy, Default)]
    struct FakeFault {
        /// Answer this message type with `Rlerror`.
        fail_type: Option<u8>,
        /// Answer every request with a tag nobody asked under.
        corrupt_tag: bool,
    }

    #[derive(Default)]
    struct ServerLog {
        versions: usize,
        attaches: usize,
        outstanding: Vec<u16>,
        peak_outstanding: usize,
        read_counts: Vec<u32>,
        clunked: Vec<u32>,
    }

    /// An in-memory 9p server reachable through the [`HostFsTransport`]
    /// interface.
    ///
    /// Every request yields once before it is answered, so joined
    /// operations really do interleave and the server sees the tags of
    /// several requests outstanding at the same time.
    #[derive(Clone)]
    struct FakeTransport {
        inner: Arc<FakeServer>,
    }

    struct FakeServer {
        msize: u32,
        depth: usize,
        fault: FakeFault,
        log: SpinMutex<ServerLog>,
    }

    impl FakeTransport {
        fn new(msize: u32, fault: FakeFault) -> Self {
            Self {
                inner: Arc::new(FakeServer {
                    msize,
                    depth: 4,
                    fault,
                    log: SpinMutex::new(ServerLog::default()),
                }),
            }
        }

        fn log(&self) -> spin::MutexGuard<'_, ServerLog> {
            self.inner.log.lock()
        }

        fn answer(&self, request: &[u8]) -> Vec<u8> {
            let ty = request[4];
            let tag = u16::from_le_bytes([request[5], request[6]]);
            let body = &request[P9_HEADER_LEN..];
            {
                let mut log = self.log();
                assert!(
                    !log.outstanding.contains(&tag),
                    "9p tag {tag} was used by two outstanding requests"
                );
                log.outstanding.push(tag);
                log.peak_outstanding = log.peak_outstanding.max(log.outstanding.len());
            }

            let reply_tag = if self.inner.fault.corrupt_tag {
                tag.wrapping_add(1)
            } else {
                tag
            };
            let mut reply = Vec::new();
            if self.inner.fault.fail_type == Some(ty) {
                // Rlerror carries a Linux errno; ENOENT will do.
                reply.extend_from_slice(&2_u32.to_le_bytes());
                return frame(P9_RLERROR, reply_tag, &reply);
            }

            match ty {
                P9_TVERSION => {
                    self.log().versions += 1;
                    let requested = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    reply.extend_from_slice(&requested.min(self.inner.msize).to_le_bytes());
                    reply.extend_from_slice(&(P9_VERSION.len() as u16).to_le_bytes());
                    reply.extend_from_slice(P9_VERSION.as_bytes());
                }
                P9_TATTACH => {
                    self.log().attaches += 1;
                    reply.extend_from_slice(&qid(P9_QTDIR, 1));
                }
                P9_TWALK => {
                    let names = u16::from_le_bytes([body[8], body[9]]);
                    reply.extend_from_slice(&names.to_le_bytes());
                    for index in 0..names {
                        reply.extend_from_slice(&qid(0, u64::from(index) + 2));
                    }
                }
                P9_TLOPEN => {
                    reply.extend_from_slice(&qid(0, 2));
                    reply.extend_from_slice(&0_u32.to_le_bytes());
                }
                P9_TGETATTR => reply.extend_from_slice(&rgetattr_body(FILE_CONTENT_LEN as u64)),
                P9_TREAD => {
                    let offset = u64::from_le_bytes([
                        body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
                    ]);
                    let count = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
                    self.log().read_counts.push(count);
                    let offset = usize::try_from(offset).expect("offset fits in usize");
                    let available = FILE_CONTENT_LEN.saturating_sub(offset);
                    let len = available.min(count as usize);
                    reply.extend_from_slice(&(len as u32).to_le_bytes());
                    reply.extend(core::iter::repeat_n(b'x', len));
                }
                P9_TWRITE => {
                    let count = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
                    reply.extend_from_slice(&count.to_le_bytes());
                }
                P9_TREADDIR => reply.extend_from_slice(&0_u32.to_le_bytes()),
                P9_TCLUNK => {
                    let fid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    self.log().clunked.push(fid);
                }
                _ => {}
            }
            frame(ty + 1, reply_tag, &reply)
        }
    }

    impl HostFsTransport for FakeTransport {
        fn mount_tag(&self) -> &str {
            "helios-test"
        }

        fn pipeline_depth(&self) -> usize {
            self.inner.depth
        }

        fn request<'a>(
            &'a self,
            bytes: &'a [u8],
            response: &'a mut BytesMut,
            response_len: usize,
        ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
            let reply = self.answer(bytes);
            let tag = u16::from_le_bytes([bytes[5], bytes[6]]);
            async move {
                // Answering only after a yield is what lets joined
                // operations overlap in the executor.
                futures_lite::future::yield_now().await;
                assert!(
                    reply.len() <= response_len,
                    "a {} byte reply does not fit the {response_len} byte response buffer",
                    reply.len()
                );
                response.clear();
                response.extend_from_slice(&reply);
                let mut log = self.inner.log.lock();
                let index = log
                    .outstanding
                    .iter()
                    .position(|outstanding| *outstanding == tag)
                    .expect("a reply must answer an outstanding request");
                log.outstanding.remove(index);
                Ok(())
            }
        }
    }

    fn frame(ty: u8, tag: u16, body: &[u8]) -> Vec<u8> {
        let mut message = Vec::with_capacity(P9_HEADER_LEN + body.len());
        let size = u32::try_from(P9_HEADER_LEN + body.len()).expect("reply fits in u32");
        message.extend_from_slice(&size.to_le_bytes());
        message.push(ty);
        message.extend_from_slice(&tag.to_le_bytes());
        message.extend_from_slice(body);
        message
    }

    fn qid(type_: u8, path: u64) -> [u8; 13] {
        let mut bytes = [0_u8; 13];
        bytes[0] = type_;
        bytes[5..].copy_from_slice(&path.to_le_bytes());
        bytes
    }

    /// The `Rgetattr` body, i.e. the reply without its 7-byte header.
    fn rgetattr_body(size: u64) -> Vec<u8> {
        let reply = rgetattr_reply(0, 2, 0o100_644, 1, size, (0, 0), (0, 0), (0, 0));
        reply[P9_HEADER_LEN..].to_vec()
    }

    fn client(msize: u32, fault: FakeFault) -> HostFsClient<FakeTransport> {
        HostFsClient::new(FakeTransport::new(msize, fault))
    }

    #[test]
    fn the_session_is_established_once_and_shared_by_every_clone() {
        let client = client(P9_REQUESTED_MSIZE, FakeFault::default());
        let clone = client.clone();
        let transport = client.inner.transport.clone();

        block_on(async {
            client.stat_path_impl("/").await.expect("root stat");
            client.stat_path_impl("/alpha").await.expect("file stat");
            clone.stat_path_impl("/beta").await.expect("clone stat");
        });

        let log = transport.log();
        assert_eq!(log.versions, 1, "one Tversion for the whole client");
        assert_eq!(log.attaches, 1, "one Tattach for the whole client");
        assert_eq!(
            client.inner.fids.live_count(),
            1,
            "only the session root fid survives an operation"
        );
        assert!(
            !log.clunked.contains(&0),
            "the session root fid is never clunked"
        );
    }

    #[test]
    fn concurrent_operations_never_share_a_tag() {
        let client = client(P9_REQUESTED_MSIZE, FakeFault::default());
        let transport = client.inner.transport.clone();

        block_on(async {
            // Establish the session first so the three operations below
            // race each other rather than the handshake.
            client.stat_path_impl("/").await.expect("root stat");
            let ((first, second), third) = zip(
                zip(
                    client.stat_path_impl("/alpha"),
                    client.stat_path_impl("/beta"),
                ),
                client.read_file_range_impl("/gamma", 0, 16),
            )
            .await;
            first.expect("first stat");
            second.expect("second stat");
            third.expect("range read");
        });

        assert!(
            transport.log().peak_outstanding > 1,
            "the operations have to overlap for the tag check to mean anything"
        );
        assert_eq!(
            client.inner.tags.live_count(),
            0,
            "every tag is returned once its reply is parsed"
        );
        assert_eq!(client.inner.fids.live_count(), 1);
    }

    #[test]
    fn a_reply_that_names_another_tag_is_rejected() {
        let client = client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                corrupt_tag: true,
                ..FakeFault::default()
            },
        );

        let error = block_on(client.stat_path_impl("/alpha"))
            .expect_err("a mismatched reply tag must not be parsed");

        assert!(matches!(error, HostFsError::Protocol(_)), "{error:?}");
    }

    #[test]
    fn fids_are_released_when_an_operation_fails() {
        let client = client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                fail_type: Some(P9_TLOPEN),
                ..FakeFault::default()
            },
        );

        block_on(client.write_file_impl("/alpha", 0, b"payload"))
            .expect_err("the server refuses to open the file");

        assert_eq!(
            client.inner.fids.live_count(),
            1,
            "the walked fid goes back to the fid space even though the open failed"
        );
        assert_eq!(client.inner.tags.live_count(), 0);
    }

    #[test]
    fn a_walk_that_fails_releases_its_fid_without_clunking_it() {
        let client = client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                fail_type: Some(P9_TWALK),
                ..FakeFault::default()
            },
        );
        let transport = client.inner.transport.clone();

        block_on(client.stat_path_impl("/alpha")).expect_err("the walk fails");

        assert_eq!(client.inner.fids.live_count(), 1);
        assert!(
            transport.log().clunked.is_empty(),
            "9p leaves the fid unattached when a walk fails, so there is nothing to clunk"
        );
    }

    #[test]
    fn the_negotiated_msize_bounds_every_read() {
        let granted = 16_384;
        let client = client(granted, FakeFault::default());
        let transport = client.inner.transport.clone();

        let bytes = block_on(client.read_file_impl("/alpha")).expect("read the whole file");

        assert_eq!(bytes.len(), FILE_CONTENT_LEN);
        let chunk = granted - P9_MAX_FIXED_HEADER_BYTES;
        let counts = transport.log().read_counts.clone();
        assert!(
            counts.iter().all(|count| *count <= chunk),
            "reads are chunked by the msize the server granted, not the one asked for: {counts:?}"
        );
        assert_eq!(
            counts.first().copied(),
            Some(chunk),
            "a read that could fill a message asks for a whole message"
        );
    }

    #[test]
    fn a_server_that_grants_an_unusable_msize_is_rejected() {
        let client = client(P9_MIN_MSIZE - 1, FakeFault::default());

        let error = block_on(client.stat_path_impl("/alpha")).expect_err("the msize is too small");

        assert!(matches!(error, HostFsError::Protocol(_)), "{error:?}");
    }
}
