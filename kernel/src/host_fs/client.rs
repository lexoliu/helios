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
//! * A session ends only when the server stops knowing it, which a
//!   machine restore can cause. [`HostFsClient::reset_session_forgotten`]
//!   then runs under the same establishment lock, so it never splits a
//!   handshake: it unpublishes the session, starts a new era in the fid
//!   space and empties the caches, all without a `Tclunk`. Readers stay
//!   lock-free. An operation in flight across the reset fails with its
//!   own error; its fids are of the old era, which the allocator
//!   recognises and ignores, and it caches nothing afterwards. The next
//!   operation establishes a new session.
//! * A VMM snapshot needs the share released: QEMU refuses to snapshot
//!   a guest whose 9p root fid is attached. [`HostFsClient::prepare_snapshot`]
//!   therefore closes a gate that every operation passes once, on entry
//!   and for its whole duration, and waits for the operations already
//!   inside to leave. Operations arriving at a closed gate park on the
//!   gate's release signal; none fails. With nothing in flight it
//!   clunks every pooled fid and the root fid, and the session stays
//!   unpublished. The gate opens again exactly when
//!   [`HostFsClient::snapshot_finished`] runs (the source machine
//!   resumes) or a machine restore reaches
//!   [`HostFileSystem::after_machine_restore`] (the restored copy); the
//!   parked operations then race for the establishment lock, and the
//!   one that wins attaches the session for all of them. Both signals
//!   are armed before the state they announce is read, so neither a
//!   release nor the last operation leaving can be missed. The gate is
//!   taken by the public operations only, never by the helpers they
//!   call, so an operation never waits on a gate it is itself holding
//!   open. The clunks the hold sends are not operations.
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
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use helios_hal::cpu::{Cpu, HardwarePerfCounters};
use helios_hal::io::IoError;
use objectpool::{Pool, ReusableObject};
use slab::Slab;
use spin::Mutex as SpinMutex;
use triomphe::Arc;

use super::cache::{CachedAttributes, HostFsCache, HostFsCacheStats, PooledReadFid};
use crate::{
    AuthorityDomain, HostDirEntry, HostDirLimit, HostFileSystem, HostFsError, HostFsErrorKind,
    HostMetadata, HostShareRestore, Notify, ObjectIdentity, ProfileSink, RawMutex,
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
/// Most path elements one `Twalk` names: `MAXWELEM` in the 9P2000
/// `walk(5)` definition and `P9_MAXWELEM` in Linux's
/// `include/net/9p/9p.h`, which servers (QEMU's `P9_MAXWELEM`) enforce.
const P9_MAXWELEM: u16 = 16;
/// Bytes a `Twalk` carries before its names:
/// `size[4] type[1] tag[2] fid[4] newfid[4] nwname[2]`.
const P9_TWALK_FIXED_LEN: usize = P9_HEADER_LEN + 4 + 4 + 2;
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

/// A 9P2000.L T-message this client sends, named by the `type` byte
/// its request frame carries.
#[derive(Clone, Copy)]
#[repr(u8)]
enum P9Message {
    Version = P9_TVERSION,
    Attach = P9_TATTACH,
    Walk = P9_TWALK,
    Read = P9_TREAD,
    Write = P9_TWRITE,
    Clunk = P9_TCLUNK,
    Lopen = P9_TLOPEN,
    Lcreate = P9_TLCREATE,
    Symlink = P9_TSYMLINK,
    Readlink = P9_TREADLINK,
    Getattr = P9_TGETATTR,
    Setattr = P9_TSETATTR,
    Readdir = P9_TREADDIR,
    Fsync = P9_TFSYNC,
    Link = P9_TLINK,
    Mkdir = P9_TMKDIR,
    Renameat = P9_TRENAMEAT,
    Unlinkat = P9_TUNLINKAT,
}

impl P9Message {
    /// The request's wire `type` byte.
    const fn wire(self) -> u8 {
        self as u8
    }

    /// The `type` byte the reply to this request carries: the request's
    /// own type plus one.
    const fn reply_wire(self) -> u8 {
        self.wire() + 1
    }

    /// The `kernel;hostfs;` perf phase one exchange of this message
    /// records under.
    const fn phase(self) -> &'static str {
        match self {
            Self::Version => "msg-version",
            Self::Attach => "msg-attach",
            Self::Walk => "msg-walk",
            Self::Read => "msg-read",
            Self::Write => "msg-write",
            Self::Clunk => "msg-clunk",
            Self::Lopen => "msg-lopen",
            Self::Lcreate => "msg-lcreate",
            Self::Symlink => "msg-symlink",
            Self::Readlink => "msg-readlink",
            Self::Getattr => "msg-getattr",
            Self::Setattr => "msg-setattr",
            Self::Readdir => "msg-readdir",
            Self::Fsync => "msg-fsync",
            Self::Link => "msg-link",
            Self::Mkdir => "msg-mkdir",
            Self::Renameat => "msg-renameat",
            Self::Unlinkat => "msg-unlinkat",
        }
    }
}

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

    /// The name bytes one `Twalk` may carry, length prefixes included.
    fn walk_name_budget(self) -> usize {
        self.message_len() - P9_TWALK_FIXED_LEN
    }

    /// `value` as a string field of a request under this session, or
    /// [`HostFsError::NameTooLong`] when the wire cannot carry it.
    fn string<'a>(self, value: &'a str) -> Result<P9Str<'a>, HostFsError> {
        P9Str::new(value, self.msize)
    }

    /// `path` as a walk under this session, every element checked.
    fn path<'a>(self, path: &'a str) -> Result<P9Path<'a>, HostFsError> {
        P9Path::new(path, self.msize)
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

    /// Returns the cell to "not established yet". The caller holds
    /// `establishing`.
    fn unpublish(&self) {
        self.published.store(0, Ordering::Release);
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
///
/// A space can be [reset](Self::reset) when the server forgets every
/// handle in it. A handle is the slab key plus the space's `base`, and a
/// reset moves `base` past every number the old era could have handed
/// out, so a handle from before the reset is recognisable by being below
/// the base: releasing it is ignored, and a new handle can never equal
/// one still held by an operation that was in flight across the reset.
struct HandleAllocator {
    live: SpinMutex<LiveHandles>,
    /// The value this space reserves — `NOTAG` or `NOFID` — and
    /// therefore the exclusive upper bound on a handle.
    reserved: u32,
    space: &'static str,
    exhausted: &'static str,
}

/// What the allocator's spin lock guards.
struct LiveHandles {
    slab: Slab<()>,
    /// The value of slab key zero in the current era.
    base: u32,
}

impl HandleAllocator {
    fn new(capacity: usize, reserved: u32, space: &'static str, exhausted: &'static str) -> Self {
        Self {
            live: SpinMutex::new(LiveHandles {
                slab: Slab::with_capacity(capacity),
                base: 0,
            }),
            reserved,
            space,
            exhausted,
        }
    }

    fn allocate(&self) -> Result<Handle<'_>, HostFsError> {
        let value = {
            let mut live = self.live.lock();
            let key = live.slab.insert(());
            match u32::try_from(key)
                .ok()
                .and_then(|key| live.base.checked_add(key))
            {
                Some(value) if value < self.reserved => value,
                _ => {
                    live.slab.remove(key);
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
        let mut live = self.live.lock();
        let Some(key) = value.checked_sub(live.base) else {
            // A handle from before a reset: the server forgot it, and
            // the number belongs to no live era.
            return;
        };
        let key = usize::try_from(key).expect("a 9p handle fits in usize");
        assert!(
            live.slab.contains(key),
            "9p {} {value} was released twice",
            self.space
        );
        live.slab.remove(key);
    }

    /// Whether `value` was handed out in the current era, which is
    /// whether the server can still know it.
    fn is_current(&self, value: u32) -> bool {
        value >= self.live.lock().base
    }

    /// Forgets every live handle and starts a new era.
    ///
    /// The base moves past the slab's capacity, which bounds every key
    /// the old era handed out. A space that has gone through so many
    /// resets that the base reaches the reserved value reports exhaustion
    /// from [`Self::allocate`].
    fn reset(&self) {
        let mut live = self.live.lock();
        let span = u32::try_from(live.slab.capacity()).unwrap_or(u32::MAX);
        live.slab.clear();
        live.base = live.base.saturating_add(span).min(self.reserved);
    }

    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.live.lock().slab.len()
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

/// The bit of [`SnapshotGate::state`] that says the share is held.
const GATE_HELD: usize = 1;
/// What one operation inside the gate adds to [`SnapshotGate::state`],
/// above the held bit.
const GATE_ONE_OPERATION: usize = 2;

/// Lets operations through until a snapshot hold closes it.
///
/// One word carries the held bit and the number of operations inside,
/// so an operation entering and the hold closing are ordered by a single
/// compare-exchange: an operation either got in before the bit was set,
/// and is counted for the drain, or sees the bit and parks. Entering and
/// leaving are lock-free; the two [`Notify`]s are broadcasts, each
/// armed before the word is read.
struct SnapshotGate {
    /// Bit 0 is [`GATE_HELD`]; the rest counts operations inside.
    state: AtomicUsize,
    /// Fired when the last operation leaves a held gate.
    drained: Notify,
    /// Fired when a held gate opens.
    released: Notify,
}

impl SnapshotGate {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            drained: Notify::new(),
            released: Notify::new(),
        }
    }

    /// Waits for the gate to be open and enters it. Any task may call it.
    async fn enter(&self) -> GateEntry<'_> {
        loop {
            let released = self.released.notified();
            let state = self.state.load(Ordering::Acquire);
            if state & GATE_HELD == 0 {
                if self
                    .state
                    .compare_exchange_weak(
                        state,
                        state + GATE_ONE_OPERATION,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return GateEntry { gate: self };
                }
                continue;
            }
            released.await;
        }
    }

    /// Closes the gate and waits for every operation inside to leave.
    /// Closing a closed gate only waits for the drain again.
    async fn close_and_drain(&self) {
        self.state.fetch_or(GATE_HELD, Ordering::AcqRel);
        loop {
            let drained = self.drained.notified();
            if self.state.load(Ordering::Acquire) >> 1 == 0 {
                return;
            }
            drained.await;
        }
    }

    /// Opens the gate, reporting whether it was closed.
    fn open(&self) -> bool {
        let was_held = self.state.fetch_and(!GATE_HELD, Ordering::AcqRel) & GATE_HELD != 0;
        if was_held {
            self.released.notify_all();
        }
        was_held
    }
}

/// One operation inside the gate; leaving it is dropping it.
struct GateEntry<'a> {
    gate: &'a SnapshotGate,
}

impl Drop for GateEntry<'_> {
    fn drop(&mut self) {
        let before = self
            .gate
            .state
            .fetch_sub(GATE_ONE_OPERATION, Ordering::AcqRel);
        if before == GATE_HELD | GATE_ONE_OPERATION {
            self.gate.drained.notify_all();
        }
    }
}

/// The clock and hardware-counter readings a profiled span is
/// measured between.
#[derive(Clone, Copy)]
struct HostFsPerfStart {
    nanos: u64,
    counters: HardwarePerfCounters,
}

/// State every clone of a client shares.
struct ClientInner<Transport: HostFsTransport, CpuImpl: Cpu + Clone> {
    transport: Transport,
    /// The platform clock the caches age their entries against. Time is
    /// a hardware capability like any other here, injected by the
    /// backend rather than read from a global.
    cpu: CpuImpl,
    /// The profile and perf histories the client's `kernel;hostfs;`
    /// phases record into — the same sink the runtime state hands the
    /// network service for its `kernel;network;` phases.
    profiles: ProfileSink,
    session: SessionCell,
    /// Closed for the length of a VMM snapshot; see the module docs.
    gate: SnapshotGate,
    tags: HandleAllocator,
    fids: HandleAllocator,
    cache: HostFsCache,
    request_buffers: Pool<BytesMut>,
    response_buffers: Pool<BytesMut>,
}

pub struct HostFsClient<Transport: HostFsTransport, CpuImpl: Cpu + Clone> {
    inner: Arc<ClientInner<Transport, CpuImpl>>,
}

impl<Transport: HostFsTransport, CpuImpl: Cpu + Clone> Clone for HostFsClient<Transport, CpuImpl> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Transport: HostFsTransport, CpuImpl: Cpu + Clone> HostFsClient<Transport, CpuImpl> {
    pub fn new(transport: Transport, cpu: CpuImpl, profiles: ProfileSink) -> Self {
        let depth = transport.pipeline_depth();
        assert!(
            depth != 0,
            "a host-fs transport must carry at least one request"
        );
        Self {
            inner: Arc::new(ClientInner {
                session: SessionCell::new(),
                gate: SnapshotGate::new(),
                tags: HandleAllocator::new(
                    depth,
                    u32::from(P9_NOTAG),
                    "tag",
                    "9p tag space exhausted",
                ),
                fids: HandleAllocator::new(depth, P9_NOFID, "fid", "9p fid space exhausted"),
                cache: HostFsCache::new(profiles.clone()),
                request_buffers: Pool::bounded(depth, BytesMut::new, reset_p9_buffer),
                response_buffers: Pool::bounded(depth, BytesMut::new, reset_p9_buffer),
                transport,
                cpu,
                profiles,
            }),
        }
    }

    /// How the session's caches have been serving the kernel.
    pub fn cache_stats(&self) -> HostFsCacheStats {
        self.inner.cache.stats()
    }

    /// Drops the session without telling the server, because the server
    /// no longer knows it: the machine came back from an image taken
    /// before the session was attached, so a `Tclunk` would name a fid
    /// that does not exist. Every cached answer and pooled fid goes with
    /// it, and the next operation runs `Tversion`/`Tattach` afresh.
    ///
    /// The session lock is held for the reset, so an establishment that
    /// is under way finishes first and none starts in the middle. An
    /// operation in flight across the reset fails with its own transport
    /// error: its fids belong to the old era, so returning them to the
    /// allocator is ignored and nothing it parked in the cache afterwards
    /// is kept.
    pub async fn reset_session_forgotten(&self) {
        let _establishing = self.inner.session.establishing.lock().await;
        self.inner.session.unpublish();
        self.inner.fids.reset();
        // The pooled read fids go unclunked: the server forgot them too.
        let forgotten = self.inner.cache.clear();
        tracing::info!(
            parked_fids = forgotten.len(),
            "9p session dropped: the server no longer knows it"
        );
    }

    /// Gives the share back to the host for a VMM snapshot and keeps it
    /// given back.
    ///
    /// The gate closes first, so no operation starts from here on, and
    /// the call waits for those already inside to finish. The session
    /// lock is taken only after the drain, because an operation inside
    /// may itself be waiting for it to establish the session. With the
    /// client quiet, every pooled read fid and then the root fid are
    /// clunked, which is what lifts QEMU's snapshot blocker, and the
    /// session is left unpublished. The hold ends in
    /// [`Self::snapshot_finished`] or in a machine restore. A caller
    /// that abandons this future after the gate closed leaves the hold
    /// in force; ending it is still one of those two.
    pub async fn prepare_snapshot(&self) {
        self.inner.gate.close_and_drain().await;
        let _establishing = self.inner.session.establishing.lock().await;
        let Some(session) = self.inner.session.get() else {
            tracing::info!("9p snapshot hold taken with no session attached");
            return;
        };
        self.inner.session.unpublish();
        let parked = self.inner.cache.clear();
        let parked_fids = parked.len();
        for fid in parked {
            self.clunk_value(fid).await;
        }
        self.clunk_value(session.root_fid).await;
        tracing::info!(parked_fids, "9p session released for a snapshot");
    }

    /// Ends the hold [`Self::prepare_snapshot`] took, because the
    /// snapshot is over and this machine runs on. A no-op when no hold
    /// is in force.
    pub fn snapshot_finished(&self) {
        if self.inner.gate.open() {
            tracing::info!("9p snapshot hold ended: the share attaches again on next use");
        }
    }

    /// Forgets every cached answer without touching the session, for a
    /// restore after which the server is the same one but the monotonic
    /// ages the entries were stamped with no longer mean what they did.
    ///
    /// A pooled read fid is still valid on the server, so it is
    /// clunked, not forgotten.
    pub async fn flush_cache(&self) {
        for fid in self.inner.cache.clear() {
            self.clunk_value(fid).await;
        }
    }

    /// Whether `session` is still the one the client holds, as opposed
    /// to one a reset dropped. An answer learned through a dropped
    /// session describes the machine before the restore and is not
    /// cached.
    fn is_current(&self, session: Session) -> bool {
        self.inner.session.get() == Some(session)
    }

    /// The monotonic reading the caches age their entries against.
    fn now_nanos(&self) -> u64 {
        crate::monotonic_nanos(&self.inner.cpu)
    }

    /// The readings a profiled span is measured from, or `None` when no
    /// profile is being collected — the disabled path pays the enable
    /// flag's atomic load and nothing else.
    fn profile_start(&self) -> Option<HostFsPerfStart> {
        self.inner.profiles.enabled().then(|| HostFsPerfStart {
            nanos: self.now_nanos(),
            counters: self.inner.cpu.hardware_perf_counters(),
        })
    }

    /// Records one `kernel;hostfs;<phase>` sample spanning `start` to
    /// now: `events` units of work taking the wall-clock span and
    /// moving `bytes` payload bytes. A profile switched off mid-span
    /// records nothing.
    fn record_phase(
        &self,
        phase: &'static str,
        start: HostFsPerfStart,
        events: usize,
        bytes: usize,
    ) {
        let Some(end) = self.profile_start() else {
            return;
        };
        let elapsed_nanos = end.nanos.saturating_sub(start.nanos);
        self.inner.profiles.record_profile_stack_parts_nanos(
            crate::ProfileScope::Kernel,
            "kernel;hostfs;",
            phase,
            elapsed_nanos,
        );
        self.inner.profiles.record_perf_metric_parts(
            crate::ProfileScope::Kernel,
            "kernel;hostfs;",
            phase,
            crate::PerfSample {
                events: crate::usize_to_u64(events, "host-fs profile event count"),
                elapsed_nanos,
                counters: end.counters.delta_since(start.counters),
                bytes: crate::usize_to_u64(bytes, "host-fs profile byte count"),
            },
        );
    }

    /// Runs `operation` as one public host-share operation: inside the
    /// snapshot gate for its whole duration, and under its
    /// `kernel;hostfs;<phase>` counter. Count is the call itself, nanos
    /// the whole wall-clock span and bytes whatever payload `payload`
    /// reads off the result — zero for an operation that moves none.
    /// Time parked at a closed gate is not part of the span.
    async fn profiled<T>(
        &self,
        phase: &'static str,
        operation: impl Future<Output = Result<T, HostFsError>> + Send,
        payload: impl FnOnce(&T) -> usize + Send,
    ) -> Result<T, HostFsError> {
        let _inside = self.inner.gate.enter().await;
        let start = self.profile_start();
        let result = operation.await;
        if let Some(start) = start {
            self.record_phase(phase, start, 1, result.as_ref().map_or(0, payload));
        }
        result
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
        let user = P9Str::new("root", msize)?;
        let mount = P9Str::new(&mount_tag, msize)?;
        self.transact(
            P9Message::Attach,
            move |body| {
                push_u32(body, root_fid);
                push_u32(body, P9_NOFID);
                push_string(body, user);
                push_string(body, mount);
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
        let dialect = P9Str::new(P9_VERSION, P9_REQUESTED_MSIZE)?;
        let response = self
            .exchange(
                P9_NOTAG,
                P9Message::Version,
                P9_DEFAULT_REQUEST_BODY_BYTES,
                |body| {
                    push_u32(body, P9_REQUESTED_MSIZE);
                    push_string(body, dialect);
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
        ty: P9Message,
        body: impl FnOnce(&mut BytesMut),
        response_len: usize,
    ) -> Result<ReusableObject<BytesMut>, HostFsError> {
        self.transact_with_capacity(ty, P9_DEFAULT_REQUEST_BODY_BYTES, body, response_len)
            .await
    }

    async fn transact_with_capacity(
        &self,
        ty: P9Message,
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
    ///
    /// The `msg-*` phase measures from just before the transport is
    /// handed the frame to just after a reply naming this request's tag
    /// arrives — the device round trip plus the waiter's wake latency.
    /// An `Rlerror` reply is still a completed round trip, so it is
    /// counted before the error is read out of the frame.
    async fn exchange(
        &self,
        tag: u16,
        ty: P9Message,
        body_capacity: usize,
        body: impl FnOnce(&mut BytesMut),
        response_len: usize,
    ) -> Result<ReusableObject<BytesMut>, HostFsError> {
        let mut request = self.inner.request_buffers.get_owned();
        let mut response = self.inner.response_buffers.get_owned();
        request.clear();
        request.reserve(P9_HEADER_LEN + body_capacity);
        request.extend_from_slice(&0_u32.to_le_bytes());
        request.extend_from_slice(&[ty.wire()]);
        request.extend_from_slice(&tag.to_le_bytes());
        body(&mut request);
        let size =
            u32::try_from(request.len()).map_err(|_| HostFsError::Protocol("request too large"))?;
        request[..4].copy_from_slice(&size.to_le_bytes());

        let submitted = self.profile_start();
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
        if let Some(start) = submitted {
            self.record_phase(ty.phase(), start, 1, 0);
        }

        let response_ty = response[4];
        if response_ty == P9_RLERROR {
            return Err(HostFsError::Server(read_u32_le(&response, P9_HEADER_LEN)?));
        }

        if response_ty != ty.reply_wire() {
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
    ///
    /// A `Twalk` names at most [`P9_MAXWELEM`] elements and must fit the
    /// msize, so a longer path is walked in steps: the first from the
    /// root fid, each later one from the fid the previous step
    /// produced, in place (`newfid` equal to `fid`, which 9p allows).
    /// Every element is checked before the first message is sent, so a
    /// name the wire cannot carry fails without touching the server.
    async fn walk(&self, session: Session, path: &str) -> Result<Handle<'_>, HostFsError> {
        let path = session.path(path)?;
        let target = self.inner.fids.allocate()?;
        let new_fid = target.value();
        let budget = session.walk_name_budget();
        let mut segments = path.segments().peekable();
        let mut parent_fid = session.root_fid;
        loop {
            // How many of the remaining elements this step takes: as
            // many as `MAXWELEM` and the message size allow. A single
            // element always fits, because a string is bounded by half
            // the payload a message carries.
            let mut count = 0_u16;
            let mut name_bytes = 0_usize;
            for segment in segments.clone() {
                let cost = segment.wire_len();
                if count == P9_MAXWELEM || name_bytes + cost > budget {
                    break;
                }
                count += 1;
                name_bytes += cost;
            }
            let step = self
                .walk_step(
                    parent_fid,
                    new_fid,
                    count,
                    name_bytes,
                    segments.by_ref().take(usize::from(count)),
                )
                .await;
            if let Err(error) = step {
                if parent_fid == new_fid {
                    // An earlier step attached `new_fid`; a failed step
                    // leaves it where that step put it, and the server
                    // still holds it.
                    self.clunk(target).await;
                }
                // A first step that fails leaves `new_fid` unattached
                // (2000.L: "if any walked element doesn't exist the
                // final fid is not set"), so dropping the handle is
                // enough.
                return Err(error);
            }
            if segments.peek().is_none() {
                return Ok(target);
            }
            parent_fid = new_fid;
        }
    }

    /// One `Twalk` of `count` elements totalling `name_bytes` encoded
    /// bytes, which must all be walked.
    async fn walk_step<'a>(
        &self,
        parent_fid: u32,
        new_fid: u32,
        count: u16,
        name_bytes: usize,
        names: impl Iterator<Item = P9Str<'a>>,
    ) -> Result<(), HostFsError> {
        let response = self
            .transact_with_capacity(
                P9Message::Walk,
                P9_TWALK_FIXED_LEN - P9_HEADER_LEN + name_bytes,
                |body| {
                    push_u32(body, parent_fid);
                    push_u32(body, new_fid);
                    push_u16(body, count);
                    for name in names {
                        push_string(body, name);
                    }
                },
                P9_RWALK_LEN,
            )
            .await?;
        let walked = read_u16_le(&response, P9_HEADER_LEN)?;
        if walked != count {
            // The server walked part of the path; the remaining element
            // doesn't exist.
            return Err(HostFsError::Transport(IoError::NotFound));
        }
        Ok(())
    }

    /// Releases a fid on both sides: `Tclunk` for the server, the
    /// handle's drop for the client's fid space.
    ///
    /// A failed clunk cannot be acted on — the operation it belonged to
    /// has already produced its result — so it is logged rather than
    /// returned.
    async fn clunk(&self, fid: Handle<'_>) {
        self.clunk_value(fid.retain()).await;
    }

    /// Releases a fid the client holds as a bare number rather than as a
    /// handle.
    ///
    /// The read-fid pool parks fids that outlive the operation which
    /// opened them, so their client-side number is retained past that
    /// operation's stack frame and released here instead.
    async fn clunk_value(&self, fid: u32) {
        if !self.inner.fids.is_current(fid) {
            // Handed out before a session reset: the server forgot it
            // along with the session, so a `Tclunk` would name nothing.
            return;
        }
        if let Err(error) = self
            .transact(
                P9Message::Clunk,
                |body| push_u32(body, fid),
                P9_SMALL_REPLY_LEN,
            )
            .await
        {
            tracing::warn!(fid, ?error, "9p clunk failed");
        }
        self.inner.fids.release(fid);
    }

    /// A fid open for reading on `path`, from the pool where one is
    /// parked and freshly walked and opened where none is.
    async fn acquire_read_fid(&self, session: Session, path: &str) -> Result<u32, HostFsError> {
        let retired = match self.inner.cache.take_read_fid(path, self.now_nanos()) {
            PooledReadFid::Reusable(fid) => return Ok(fid),
            PooledReadFid::Retired(fid) => Some(fid),
            PooledReadFid::Absent => None,
        };
        if let Some(fid) = retired {
            self.clunk_value(fid).await;
        }
        let target = self.walk(session, path).await?;
        match self.open(target.value(), P9_DOTL_RDONLY).await {
            Ok(()) => Ok(target.retain()),
            Err(error) => {
                self.clunk(target).await;
                Err(error)
            }
        }
    }

    /// Parks a read fid for the next read of `path`, clunking whatever
    /// the pool had to give up to make room for it.
    async fn release_read_fid(&self, session: Session, path: &str, fid: u32) {
        if !self.is_current(session) {
            // The session was dropped while the fid was in use: the
            // server forgot it, so there is nothing to park or clunk.
            return;
        }
        if let Some(orphaned) = self.inner.cache.park_read_fid(path, fid, self.now_nanos()) {
            self.clunk_value(orphaned).await;
        }
    }

    /// Forgets what the caches knew about a file whose contents or
    /// attributes the kernel just changed.
    async fn invalidate_contents(&self, path: &str) {
        if let Some(orphaned) = self.inner.cache.invalidate_contents(path) {
            self.clunk_value(orphaned).await;
        }
    }

    /// Forgets what the caches knew about a name the kernel just
    /// created, removed, moved or linked, and about its directory.
    async fn invalidate_entry(&self, path: &str) {
        if let Some(orphaned) = self.inner.cache.invalidate_entry(path) {
            self.clunk_value(orphaned).await;
        }
    }

    /// Remembers what the host answered about `path`, including that it
    /// answered "no such path".
    fn record_stat(
        &self,
        session: Session,
        path: &str,
        result: &Result<HostMetadata, HostFsError>,
    ) {
        if !self.is_current(session) {
            return;
        }
        let now_nanos = self.now_nanos();
        match result {
            Ok(metadata) => self
                .inner
                .cache
                .insert_attributes(path, metadata, now_nanos),
            Err(error) if error.kind() == HostFsErrorKind::NoEntry => {
                self.inner.cache.insert_missing(path, now_nanos);
            }
            Err(_) => {}
        }
    }

    async fn get_attr(&self, fid: u32) -> Result<HostMetadata, HostFsError> {
        let response = self
            .transact(
                P9Message::Getattr,
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
                P9Message::Read,
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
                P9Message::Read,
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

    /// Reads `fid` — already open for reading — from the start.
    ///
    /// `expected_size` plans the reads and sizes the buffer; it is what
    /// the host last said, which a host-side truncation may already have
    /// outlived. A zero-length `Rread` is therefore end of file rather
    /// than a protocol fault, and the caller gets the bytes that were
    /// actually there.
    async fn read_file_all(
        &self,
        session: Session,
        fid: u32,
        expected_size: u64,
    ) -> Result<Vec<u8>, HostFsError> {
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
                tracing::debug!(
                    read_bytes = bytes.len(),
                    expected_size,
                    "the host share reached end of file before the size it reported"
                );
                break;
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
        limit: HostDirLimit,
    ) -> Result<Vec<HostDirEntry>, HostFsError> {
        self.open(fid, P9_DOTL_RDONLY | P9_DOTL_DIRECTORY).await?;

        let batch = u32::try_from(session.payload_chunk()).expect("a readdir batch fits in u32");
        let mut entries = Vec::new();
        let mut name_bytes = 0_usize;
        let mut offset = 0_u64;
        loop {
            let response = self
                .transact(
                    P9Message::Readdir,
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
            // The count is the server's: it must lie inside the reply, and
            // the entries must end exactly on it.
            let end = cursor
                .checked_add(count)
                .filter(|end| *end <= response.len())
                .ok_or(HostFsError::Protocol(
                    "readdir payload count exceeds the reply",
                ))?;
            while cursor < end {
                let qid_type = read_u8(&response, cursor)?;
                cursor += 1;
                cursor += 4;
                cursor += 8;
                // The offset is the cookie the next `Treaddir` resumes from.
                // One that does not move past the offset this batch was
                // requested at, or past the entry before it, would have the
                // server answer the same entries again forever.
                let entry_offset = read_u64_le(&response, cursor)?;
                if entry_offset <= offset {
                    return Err(HostFsError::Protocol(
                        "readdir offsets did not strictly increase",
                    ));
                }
                offset = entry_offset;
                cursor += 8;
                let type_ = read_u8(&response, cursor)?;
                cursor += 1;
                let name = read_string(&response, &mut cursor)?;
                if cursor > end {
                    return Err(HostFsError::Protocol("readdir entry overran its payload"));
                }
                if name == "." || name == ".." {
                    continue;
                }
                name_bytes = name_bytes.saturating_add(name.len());
                limit.check(entries.len() + 1, name_bytes)?;
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
            P9Message::Lopen,
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
                    P9Message::Write,
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
        match self.inner.cache.attributes(path, self.now_nanos()) {
            CachedAttributes::Present(metadata) => return Ok(metadata),
            CachedAttributes::Absent => return Err(HostFsError::Transport(IoError::NotFound)),
            CachedAttributes::Unknown => {}
        }
        let session = self.session().await?;
        let target = self.walk(session, path).await;
        let metadata = match target {
            Ok(target) => {
                let metadata = self.get_attr(target.value()).await;
                self.clunk(target).await;
                metadata
            }
            Err(error) => Err(error),
        };
        self.record_stat(session, path, &metadata);
        metadata
    }

    async fn read_dir_impl(
        &self,
        path: &str,
        limit: HostDirLimit,
    ) -> Result<Vec<HostDirEntry>, HostFsError> {
        if let Some(entries) = self.inner.cache.directory(path, self.now_nanos()) {
            // A listing cached for an earlier caller may be past this one's limit.
            let name_bytes: usize = entries.iter().map(|entry| entry.name.len()).sum();
            limit.check(entries.len(), name_bytes)?;
            return Ok(entries);
        }
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let entries = self.read_dir_entries(session, target.value(), limit).await;
        self.clunk(target).await;
        if let Ok(entries) = &entries
            && self.is_current(session)
        {
            self.inner
                .cache
                .insert_directory(path, entries, self.now_nanos());
        }
        entries
    }

    async fn read_file_impl(&self, path: &str) -> Result<Vec<u8>, HostFsError> {
        let session = self.session().await?;
        let cached_size = match self.inner.cache.attributes(path, self.now_nanos()) {
            CachedAttributes::Present(metadata) => Some(metadata.size),
            CachedAttributes::Absent => return Err(HostFsError::Transport(IoError::NotFound)),
            CachedAttributes::Unknown => None,
        };
        let fid = match self.acquire_read_fid(session, path).await {
            Ok(fid) => fid,
            Err(error) => {
                self.record_stat(session, path, &Err(error.clone()));
                tracing::warn!("host-fs read_file({path}) failed: {error:?}");
                return Err(error);
            }
        };
        let data = async {
            let size = match cached_size {
                Some(size) => size,
                None => {
                    let metadata = self.get_attr(fid).await?;
                    let size = metadata.size;
                    self.record_stat(session, path, &Ok(metadata));
                    size
                }
            };
            self.read_file_all(session, fid, size).await
        }
        .await;
        if let Err(error) = &data {
            tracing::warn!("host-fs read_file({path}) failed: {error:?}");
        }
        self.release_read_fid(session, path, fid).await;
        data
    }

    async fn read_file_range_impl(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, HostFsError> {
        let session = self.session().await?;
        let fid = self.acquire_read_fid(session, path).await?;
        // The reply has to fit in one message, so a caller asking for
        // more than that gets a short read rather than a request the
        // server would have to truncate.
        let bounded = max_bytes
            .min(u32::try_from(session.payload_chunk()).expect("a payload chunk fits in u32"));
        let data = self.read_chunk(session, fid, offset, bounded).await;
        if let Err(error) = &data {
            tracing::warn!(
                "host-fs read_file_range(path={path}, offset={offset}, max_bytes={max_bytes}) failed: {error:?}"
            );
        }
        self.release_read_fid(session, path, fid).await;
        data
    }

    async fn create_directory_impl(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let session = self.session().await?;
        let name = session.string(name)?;
        let directory = self.walk(session, parent).await?;
        let fid = directory.value();
        let result = self
            .transact(
                P9Message::Mkdir,
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
        self.invalidate_entry(path).await;
        result
    }

    async fn create_file_impl(&self, path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(path)?;
        let session = self.session().await?;
        let name = session.string(name)?;
        let directory = self.walk(session, parent).await?;
        let fid = directory.value();
        let result = self
            .transact(
                P9Message::Lcreate,
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
        self.invalidate_entry(path).await;
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
                P9Message::Setattr,
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
        self.invalidate_contents(path).await;
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
                P9Message::Setattr,
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
        self.invalidate_contents(path).await;
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
        self.invalidate_contents(path).await;
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
        self.invalidate_contents(path).await;
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
                P9Message::Fsync,
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
        let name = session.string(name)?;
        let dir = self.walk(session, parent).await?;
        let fid = dir.value();
        let flags = if directory { P9_DOTL_AT_REMOVEDIR } else { 0 };
        let result = self
            .transact(
                P9Message::Unlinkat,
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
        self.invalidate_entry(path).await;
        result
    }

    async fn rename_impl(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (source_parent, source_name) = split_parent_name(source)?;
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let session = self.session().await?;
        let source_name = session.string(source_name)?;
        let destination_name = session.string(destination_name)?;
        session.path(source_parent)?;
        session.path(destination_parent)?;
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
                P9Message::Renameat,
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
        self.invalidate_entry(source).await;
        self.invalidate_entry(destination).await;
        result
    }

    async fn hard_link_impl(&self, source: &str, destination: &str) -> Result<(), HostFsError> {
        let (destination_parent, destination_name) = split_parent_name(destination)?;
        let session = self.session().await?;
        let destination_name = session.string(destination_name)?;
        session.path(source)?;
        session.path(destination_parent)?;
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
                P9Message::Link,
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
        // The new name is a new directory entry; the old one keeps its
        // contents but gains a link, which its cached attributes no
        // longer report.
        self.invalidate_entry(destination).await;
        self.invalidate_contents(source).await;
        result
    }

    async fn symlink_impl(&self, target: &str, link_path: &str) -> Result<(), HostFsError> {
        let (parent, name) = split_parent_name(link_path)?;
        let session = self.session().await?;
        let name = session.string(name)?;
        let target = session.string(target)?;
        let directory = self.walk(session, parent).await?;
        let fid = directory.value();
        let result = self
            .transact(
                P9Message::Symlink,
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
        self.invalidate_entry(link_path).await;
        result
    }

    async fn read_link_impl(&self, path: &str) -> Result<String, HostFsError> {
        let session = self.session().await?;
        let target = self.walk(session, path).await?;
        let fid = target.value();
        let result = self
            .transact(
                P9Message::Readlink,
                |body| push_u32(body, fid),
                P9_RLINK_LEN,
            )
            .await
            .and_then(|response| {
                let mut cursor = P9_HEADER_LEN;
                read_string(&response, &mut cursor)
            });
        self.clunk(target).await;
        result
    }
}

impl<Transport: HostFsTransport, CpuImpl: Cpu + Clone> HostFileSystem
    for HostFsClient<Transport, CpuImpl>
{
    fn cache_stats(&self) -> Option<HostFsCacheStats> {
        Some(self.cache_stats())
    }

    async fn after_machine_restore(&self, restore: HostShareRestore) {
        match restore {
            HostShareRestore::ServerKept => self.flush_cache().await,
            HostShareRestore::SessionLost => self.reset_session_forgotten().await,
        }
        // A restored copy never sees `snapshot-finished`: its machine
        // generation advancing is what ends a hold taken before the
        // snapshot, after the session state above is settled.
        self.snapshot_finished();
    }

    async fn prepare_snapshot(&self) {
        HostFsClient::prepare_snapshot(self).await;
    }

    fn snapshot_finished(&self) {
        HostFsClient::snapshot_finished(self);
    }

    async fn stat_path<'a>(&'a self, path: &'a str) -> Result<HostMetadata, HostFsError> {
        self.profiled("op-stat", self.stat_path_impl(path), |_| 0)
            .await
    }

    async fn read_dir<'a>(
        &'a self,
        path: &'a str,
        limit: HostDirLimit,
    ) -> Result<Vec<HostDirEntry>, HostFsError> {
        self.profiled("op-read-dir", self.read_dir_impl(path, limit), |entries| {
            entries.iter().map(|entry| entry.name.len()).sum()
        })
        .await
    }

    async fn read_file<'a>(&'a self, path: &'a str) -> Result<Vec<u8>, HostFsError> {
        self.profiled("op-read-file", self.read_file_impl(path), |data| data.len())
            .await
    }

    async fn read_file_range<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, HostFsError> {
        self.profiled(
            "op-read-range",
            self.read_file_range_impl(path, offset, max_bytes),
            |data| data.len(),
        )
        .await
    }

    async fn write_file<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> Result<(), HostFsError> {
        self.profiled(
            "op-write",
            self.write_file_impl(path, offset, bytes),
            |_| bytes.len(),
        )
        .await
    }

    async fn append_file<'a>(&'a self, path: &'a str, bytes: &'a [u8]) -> Result<u64, HostFsError> {
        self.profiled("op-append", self.append_file_impl(path, bytes), |_| {
            bytes.len()
        })
        .await
    }

    async fn sync_file<'a>(&'a self, path: &'a str) -> Result<(), HostFsError> {
        self.profiled("op-sync", self.sync_file_impl(path), |_| 0)
            .await
    }

    async fn truncate_file<'a>(&'a self, path: &'a str) -> Result<(), HostFsError> {
        self.profiled("op-truncate", self.truncate_file_impl(path), |_| 0)
            .await
    }

    async fn set_file_size<'a>(&'a self, path: &'a str, size: u64) -> Result<(), HostFsError> {
        self.profiled("op-set-size", self.set_file_size_impl(path, size), |_| 0)
            .await
    }

    async fn set_times<'a>(
        &'a self,
        path: &'a str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
    ) -> Result<(), HostFsError> {
        self.profiled(
            "op-set-times",
            self.set_times_impl(path, access_nanos, modified_nanos),
            |_| 0,
        )
        .await
    }

    async fn create_file<'a>(&'a self, path: &'a str) -> Result<(), HostFsError> {
        self.profiled("op-create-file", self.create_file_impl(path), |_| 0)
            .await
    }

    async fn create_directory<'a>(&'a self, path: &'a str) -> Result<(), HostFsError> {
        self.profiled("op-create-dir", self.create_directory_impl(path), |_| 0)
            .await
    }

    async fn remove<'a>(&'a self, path: &'a str, directory: bool) -> Result<(), HostFsError> {
        self.profiled("op-remove", self.remove_impl(path, directory), |_| 0)
            .await
    }

    async fn rename<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
    ) -> Result<(), HostFsError> {
        self.profiled("op-rename", self.rename_impl(source, destination), |_| 0)
            .await
    }

    async fn hard_link<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
    ) -> Result<(), HostFsError> {
        self.profiled(
            "op-hard-link",
            self.hard_link_impl(source, destination),
            |_| 0,
        )
        .await
    }

    async fn symlink<'a>(&'a self, target: &'a str, link_path: &'a str) -> Result<(), HostFsError> {
        self.profiled("op-symlink", self.symlink_impl(target, link_path), |_| 0)
            .await
    }

    async fn read_link<'a>(&'a self, path: &'a str) -> Result<String, HostFsError> {
        self.profiled("op-read-link", self.read_link_impl(path), |target| {
            target.len()
        })
        .await
    }
}

fn path_segments(path: &str) -> impl Iterator<Item = &str> + Clone {
    path.split('/').filter(|segment| !segment.is_empty())
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

fn push_string(buf: &mut BytesMut, value: P9Str<'_>) {
    push_u16(buf, value.len());
    buf.extend_from_slice(value.as_str().as_bytes());
}

mod wire_string {
    //! The two guest-supplied shapes a request's string fields take, built
    //! only through a check that the wire can carry them. The fields are
    //! private, so an encoder holding a [`P9Str`] has no length left to
    //! verify.

    use super::{HostFsError, P9_MAX_FIXED_HEADER_BYTES, path_segments};

    /// The longest string field a request under `msize` carries.
    ///
    /// A string is `len[2]` plus bytes, so no field exceeds `u16::MAX`.
    /// The message-size bound is half of what follows the fixed prefix
    /// ([`P9_MAX_FIXED_HEADER_BYTES`], the longest any message has): the
    /// messages that carry two strings (`Tattach`, `Tsymlink`,
    /// `Trenameat`) then fit whatever the pair.
    pub(super) fn max_string_len(msize: u32) -> u16 {
        let by_message = msize.saturating_sub(P9_MAX_FIXED_HEADER_BYTES) / 2;
        u16::try_from(by_message).unwrap_or(u16::MAX)
    }

    /// A string field of a request: at most `max_string_len` bytes.
    #[derive(Clone, Copy)]
    pub(super) struct P9Str<'a> {
        value: &'a str,
        len: u16,
    }

    impl<'a> P9Str<'a> {
        /// `value` as a string field of a message no longer than `msize`.
        pub(super) fn new(value: &'a str, msize: u32) -> Result<Self, HostFsError> {
            let max_len = max_string_len(msize);
            match u16::try_from(value.len()) {
                Ok(len) if len <= max_len => Ok(Self { value, len }),
                _ => Err(HostFsError::NameTooLong {
                    len: value.len(),
                    max_len: usize::from(max_len),
                }),
            }
        }

        pub(super) fn as_str(self) -> &'a str {
            self.value
        }

        /// The `len[2]` prefix the field is sent with.
        pub(super) fn len(self) -> u16 {
            self.len
        }

        /// What the field occupies on the wire: prefix and bytes.
        pub(super) fn wire_len(self) -> usize {
            2 + usize::from(self.len)
        }
    }

    /// A path whose every element is a [`P9Str`] under one msize.
    #[derive(Clone, Copy)]
    pub(super) struct P9Path<'a> {
        path: &'a str,
    }

    impl<'a> P9Path<'a> {
        /// `path` as a walk under `msize`, or the first element the wire
        /// cannot carry.
        pub(super) fn new(path: &'a str, msize: u32) -> Result<Self, HostFsError> {
            for segment in path_segments(path) {
                P9Str::new(segment, msize)?;
            }
            Ok(Self { path })
        }

        /// The path's elements, in order.
        pub(super) fn segments(self) -> impl Iterator<Item = P9Str<'a>> + Clone {
            path_segments(self.path).map(|value| P9Str {
                value,
                // `new` checked every element against `max_string_len`,
                // which is at most `u16::MAX`.
                len: value.len() as u16,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn string_bound_follows_the_message_size() {
            assert_eq!(max_string_len(u32::MAX), u16::MAX);
            assert_eq!(max_string_len(8192), (8192 - 23) / 2);
        }
    }
}
use wire_string::{P9Path, P9Str};

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
    use super::wire_string::max_string_len;
    use super::*;
    use crate::test_support::ManualClockCpu;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once, zip};

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
        assert_eq!(path_segments("/alpha//beta/gamma/").count(), 3);
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

    /// The three 9P2000.L timestamps of an `Rgetattr` reply, each a
    /// `(seconds, nanoseconds)` pair.
    struct RgetattrTimes {
        access: (u64, u64),
        modified: (u64, u64),
        status: (u64, u64),
    }

    impl RgetattrTimes {
        /// The timestamps a test that does not look at them uses.
        const ZERO: Self = Self {
            access: (0, 0),
            modified: (0, 0),
            status: (0, 0),
        };
    }

    /// Builds an `Rgetattr` reply with the 9P2000.L field layout.
    fn rgetattr_reply(
        qid_type: u8,
        qid_path: u64,
        mode: u32,
        link_count: u64,
        size: u64,
        times: RgetattrTimes,
    ) -> alloc::vec::Vec<u8> {
        let RgetattrTimes {
            access,
            modified,
            status,
        } = times;
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
            RgetattrTimes {
                access: (11, 12),
                modified: (13, 14),
                status: (15, 16),
            },
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
        let reply = rgetattr_reply(P9_QTDIR, 7, 0o040_755, 2, 0, RgetattrTimes::ZERO);

        let metadata = parse_getattr_reply(&reply).expect("Rgetattr reply should decode");

        assert_eq!(metadata.qid_type & P9_QTDIR, P9_QTDIR);
        assert_eq!(metadata.identity.local(), 7);
    }

    #[test]
    fn getattr_reply_shorter_than_the_fixed_layout_is_rejected() {
        let mut reply = rgetattr_reply(0, 1, 0, 1, 0, RgetattrTimes::ZERO);
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
        /// How `Treaddir` is answered, when not with an empty batch.
        readdir: ReaddirFault,
        /// Answer every `Twalk` past this many with `Rlerror`.
        refuse_walks_after: Option<usize>,
    }

    /// A misbehaving answer to `Treaddir`.
    #[derive(Clone, Copy, Default)]
    enum ReaddirFault {
        /// An empty batch: the end of the directory.
        #[default]
        None,
        /// The same one-entry batch every time, whatever offset was asked.
        RepeatBatch,
        /// A fresh one-entry batch every time, with offsets that rise, so
        /// the directory never ends.
        Endless,
    }

    /// A limit far above anything the happy-path tests list.
    const TEST_DIR_LIMIT: HostDirLimit = HostDirLimit {
        max_entries: 1 << 16,
        max_name_bytes: 1 << 20,
    };

    /// One `Treaddir` entry: qid, offset, type, name.
    fn dirent(offset: u64, name: &str) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(&qid(0, offset));
        entry.extend_from_slice(&offset.to_le_bytes());
        entry.push(8); // DT_REG
        entry.extend_from_slice(&(name.len() as u16).to_le_bytes());
        entry.extend_from_slice(name.as_bytes());
        entry
    }

    /// An `Rreaddir` body carrying `entries`.
    fn readdir_body(entries: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        body.extend_from_slice(entries);
        body
    }

    #[derive(Default)]
    struct ServerLog {
        versions: usize,
        attaches: usize,
        walks: usize,
        opens: usize,
        getattrs: usize,
        readdirs: usize,
        outstanding: Vec<u16>,
        peak_outstanding: usize,
        read_counts: Vec<u32>,
        clunked: Vec<u32>,
        /// The type of every request, in arrival order.
        types: Vec<u8>,
        /// `(fid, newfid, nwname)` of every `Twalk`, in arrival order.
        walk_requests: Vec<(u32, u32, u16)>,
    }

    impl ServerLog {
        /// Counts one arriving request by its message type.
        fn record(&mut self, ty: u8) {
            self.types.push(ty);
            let counter = match ty {
                P9_TVERSION => &mut self.versions,
                P9_TATTACH => &mut self.attaches,
                P9_TWALK => &mut self.walks,
                P9_TLOPEN => &mut self.opens,
                P9_TGETATTR => &mut self.getattrs,
                P9_TREADDIR => &mut self.readdirs,
                _ => return,
            };
            *counter += 1;
        }
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
            assert!(
                request.len() <= self.inner.msize as usize,
                "a {} byte request exceeds the {} byte msize",
                request.len(),
                self.inner.msize
            );
            {
                let mut log = self.log();
                assert!(
                    !log.outstanding.contains(&tag),
                    "9p tag {tag} was used by two outstanding requests"
                );
                log.outstanding.push(tag);
                log.peak_outstanding = log.peak_outstanding.max(log.outstanding.len());
                // Counted where the request arrives rather than where it
                // is answered, so a request the fault injector refuses
                // still shows up as one the client chose to send.
                log.record(ty);
            }

            let reply_tag = if self.inner.fault.corrupt_tag {
                tag.wrapping_add(1)
            } else {
                tag
            };
            let mut reply = Vec::new();
            let walk_refused = ty == P9_TWALK
                && self
                    .inner
                    .fault
                    .refuse_walks_after
                    .is_some_and(|after| self.log().walks > after);
            if self.inner.fault.fail_type == Some(ty) || walk_refused {
                // Rlerror carries a Linux errno; ENOENT will do.
                reply.extend_from_slice(&2_u32.to_le_bytes());
                return frame(P9_RLERROR, reply_tag, &reply);
            }

            match ty {
                P9_TVERSION => {
                    let requested = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    reply.extend_from_slice(&requested.min(self.inner.msize).to_le_bytes());
                    reply.extend_from_slice(&(P9_VERSION.len() as u16).to_le_bytes());
                    reply.extend_from_slice(P9_VERSION.as_bytes());
                }
                P9_TATTACH => reply.extend_from_slice(&qid(P9_QTDIR, 1)),
                P9_TWALK => {
                    let names = u16::from_le_bytes([body[8], body[9]]);
                    self.log().walk_requests.push((
                        u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
                        u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                        names,
                    ));
                    reply.extend_from_slice(&names.to_le_bytes());
                    for index in 0..names {
                        reply.extend_from_slice(&qid(0, u64::from(index) + 2));
                    }
                }
                P9_TLOPEN => {
                    reply.extend_from_slice(&qid(0, 2));
                    reply.extend_from_slice(&0_u32.to_le_bytes());
                }
                P9_TGETATTR => {
                    reply.extend_from_slice(&rgetattr_body(FILE_CONTENT_LEN as u64));
                }
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
                P9_TREADDIR => match self.inner.fault.readdir {
                    ReaddirFault::None => reply.extend_from_slice(&0_u32.to_le_bytes()),
                    ReaddirFault::RepeatBatch => {
                        reply.extend_from_slice(&readdir_body(&dirent(7, "same")));
                    }
                    ReaddirFault::Endless => {
                        let offset = u64::from_le_bytes([
                            body[4], body[5], body[6], body[7], body[8], body[9], body[10],
                            body[11],
                        ]) + 1;
                        reply.extend_from_slice(&readdir_body(&dirent(
                            offset,
                            &alloc::format!("entry-{offset}"),
                        )));
                    }
                },
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
        let reply = rgetattr_reply(0, 2, 0o100_644, 1, size, RgetattrTimes::ZERO);
        reply[P9_HEADER_LEN..].to_vec()
    }

    /// A client, its fake server and the clock its caches age against.
    struct Fixture {
        client: HostFsClient<FakeTransport, ManualClockCpu>,
        transport: FakeTransport,
        clock: ManualClockCpu,
    }

    fn client(msize: u32, fault: FakeFault) -> HostFsClient<FakeTransport, ManualClockCpu> {
        fixture(msize, fault).client
    }

    fn fixture(msize: u32, fault: FakeFault) -> Fixture {
        let transport = FakeTransport::new(msize, fault);
        let clock = ManualClockCpu::new();
        Fixture {
            client: HostFsClient::new(
                transport.clone(),
                clock.clone(),
                crate::test_support::test_profile_sink(),
            ),
            transport,
            clock,
        }
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
        assert_eq!(
            client.inner.fids.live_count(),
            2,
            "the session root fid, plus the read fid the pool parked for the range read"
        );
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

    /// The `Twalk`s the server saw for a stat of a path of `depth`
    /// single-letter components, and the session's root fid.
    fn walks_for_depth(depth: usize) -> (u32, Vec<(u32, u32, u16)>) {
        let client = client(P9_REQUESTED_MSIZE, FakeFault::default());
        let transport = client.inner.transport.clone();
        let path = (0..depth).fold(String::new(), |path, _| path + "/a");

        block_on(client.stat_path_impl(&path)).expect("every step of the walk succeeds");

        assert_eq!(
            client.inner.fids.live_count(),
            1,
            "only the session root fid survives the stat"
        );
        let root = client
            .inner
            .session
            .get()
            .expect("the stat opened a session");
        let walks = transport.log().walk_requests.clone();
        (root.root_fid, walks)
    }

    #[test]
    fn a_walk_past_maxwelem_is_split_across_twalks_chained_from_the_new_fid() {
        let (root, walks) = walks_for_depth(0);
        let target = walks[0].1;
        assert_eq!(walks, [(root, target, 0)], "an empty path clones the root");

        let (root, walks) = walks_for_depth(16);
        let target = walks[0].1;
        assert_eq!(walks, [(root, target, 16)], "MAXWELEM names fit one Twalk");

        let (root, walks) = walks_for_depth(17);
        let target = walks[0].1;
        assert_eq!(walks, [(root, target, 16), (target, target, 1)]);

        let (root, walks) = walks_for_depth(40);
        let target = walks[0].1;
        assert_eq!(
            walks,
            [
                (root, target, 16),
                (target, target, 16),
                (target, target, 8)
            ]
        );
    }

    #[test]
    fn a_later_twalk_that_fails_clunks_the_fid_an_earlier_one_attached() {
        let client = client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                refuse_walks_after: Some(1),
                ..FakeFault::default()
            },
        );
        let transport = client.inner.transport.clone();
        let path = (0..20).fold(String::new(), |path, _| path + "/a");

        block_on(client.stat_path_impl(&path)).expect_err("the second Twalk is refused");

        let log = transport.log();
        let target = log.walk_requests[0].1;
        assert_eq!(log.walks, 2, "the refused Twalk was the second one sent");
        assert_eq!(
            log.clunked,
            [target],
            "the fid the first Twalk attached is clunked once"
        );
        assert_eq!(client.inner.fids.live_count(), 1);
        assert_eq!(client.inner.tags.live_count(), 0);
    }

    #[test]
    fn a_walk_whose_names_overflow_the_msize_is_split_by_size() {
        let client = client(P9_MIN_MSIZE, FakeFault::default());
        let transport = client.inner.transport.clone();
        // Two of these fill one Twalk of the 8192-byte msize
        // (2 * (2 + 4084) = 8172 <= 8192 - 17) and a third does not.
        let name = "n".repeat(usize::from(max_string_len(P9_MIN_MSIZE)));
        let path = alloc::format!("/{name}/{name}/{name}");

        block_on(client.stat_path_impl(&path)).expect("the walk succeeds across two Twalks");

        let log = transport.log();
        let counts = log
            .walk_requests
            .iter()
            .map(|walk| walk.2)
            .collect::<Vec<_>>();
        assert_eq!(counts, [2, 1]);
    }

    #[test]
    fn a_name_the_wire_cannot_carry_is_refused_before_anything_is_sent() {
        let client = client(P9_REQUESTED_MSIZE, FakeFault::default());
        let transport = client.inner.transport.clone();
        let longest = "a".repeat(usize::from(u16::MAX));
        let too_long = "a".repeat(usize::from(u16::MAX) + 1);

        block_on(client.session()).expect("the session opens");
        let sent_by_the_handshake = transport.log().types.clone();
        assert_eq!(sent_by_the_handshake, [P9_TVERSION, P9_TATTACH]);

        // The name of a created file, a walked component, both names of
        // a rename and a symlink's target are all string fields.
        let results = [
            block_on(client.create_file_impl(&alloc::format!("/dir/{too_long}"))),
            block_on(client.create_directory_impl(&alloc::format!("/{too_long}"))),
            block_on(client.remove_impl(&alloc::format!("/{too_long}"), false)),
            block_on(client.stat_path_impl(&alloc::format!("/dir/{too_long}/leaf"))).map(|_| ()),
            block_on(client.rename_impl("/a", &alloc::format!("/{too_long}"))),
            block_on(client.rename_impl(&alloc::format!("/{too_long}/b"), "/a")),
            block_on(client.hard_link_impl("/a", &alloc::format!("/{too_long}"))),
            block_on(client.hard_link_impl(&alloc::format!("/{too_long}"), "/a")),
            block_on(client.symlink_impl(&too_long, "/link")),
            block_on(client.symlink_impl("/target", &alloc::format!("/{too_long}"))),
        ];
        for result in results {
            let error = result.expect_err("a 65536-byte name cannot be encoded");
            assert!(
                matches!(
                    error,
                    HostFsError::NameTooLong {
                        len: 65_536,
                        max_len: 65_535
                    }
                ),
                "{error:?}"
            );
            assert_eq!(error.kind(), HostFsErrorKind::NameTooLong);
        }

        assert_eq!(
            transport.log().types,
            sent_by_the_handshake,
            "no refused name put a request on the wire"
        );
        assert_eq!(client.inner.fids.live_count(), 1);

        // The longest name the format holds is still carried.
        block_on(client.create_file_impl(&alloc::format!("/dir/{longest}")))
            .expect("a 65535-byte name fits");
        assert!(transport.log().types.contains(&P9_TLCREATE));
    }

    #[test]
    fn a_name_is_bounded_by_the_negotiated_msize() {
        let client = client(P9_MIN_MSIZE, FakeFault::default());
        let transport = client.inner.transport.clone();
        let longest = usize::from(max_string_len(P9_MIN_MSIZE));

        let error =
            block_on(client.create_file_impl(&alloc::format!("/{}", "a".repeat(longest + 1))))
                .expect_err("the request would not fit the msize");
        assert!(
            matches!(error, HostFsError::NameTooLong { len, max_len } if len == longest + 1 && max_len == longest),
            "{error:?}"
        );
        assert!(!transport.log().types.contains(&P9_TLCREATE));

        block_on(client.create_file_impl(&alloc::format!("/{}", "a".repeat(longest))))
            .expect("the longest name the msize allows is carried");
        assert!(transport.log().types.contains(&P9_TLCREATE));
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
    fn a_repeated_stat_asks_the_host_once_inside_the_ttl() {
        let Fixture {
            client,
            transport,
            clock,
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.stat_path_impl("/alpha").await.expect("first stat");
            clock.advance(super::super::cache::CACHE_TTL_NANOS - 1);
            client.stat_path_impl("/alpha").await.expect("cached stat");
        });
        assert_eq!(
            transport.log().getattrs,
            1,
            "the second stat is inside the TTL, so it never reaches the host"
        );

        block_on(async {
            clock.advance(1);
            client.stat_path_impl("/alpha").await.expect("expired stat");
        });
        assert_eq!(
            transport.log().getattrs,
            2,
            "past the TTL the host is asked again"
        );
        assert_eq!(client.cache_stats().attribute_hits, 1);
        assert_eq!(client.cache_stats().attribute_misses, 2);
    }

    #[test]
    fn a_session_the_server_forgot_is_dropped_without_a_clunk_and_attached_again() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.stat_path_impl("/alpha").await.expect("first stat");
            client.read_file_impl("/alpha").await.expect("first read");
        });
        let clunked_before = {
            let log = transport.log();
            assert_eq!((log.versions, log.attaches), (1, 1));
            assert_eq!(log.getattrs, 1, "the read's size probe came from the cache");
            log.clunked.clone()
        };

        block_on(client.reset_session_forgotten());
        assert_eq!(
            transport.log().clunked,
            clunked_before,
            "the server forgot the fids, so the reset clunks none"
        );
        assert_eq!(client.inner.fids.live_count(), 0);
        assert_eq!(client.inner.session.get(), None);

        block_on(async {
            client.stat_path_impl("/alpha").await.expect("stat after");
        });
        let log = transport.log();
        assert_eq!(
            (log.versions, log.attaches),
            (2, 2),
            "the next operation sends Tversion and Tattach"
        );
        assert_eq!(
            log.getattrs, 2,
            "attributes cached before the reset are not served"
        );
        drop(log);
        assert_eq!(client.inner.fids.live_count(), 1, "only the new root fid");
    }

    /// The reactions task with its host-share step over a real client,
    /// and every other step doing nothing.
    struct HostShareSteps {
        client: HostFsClient<FakeTransport, ManualClockCpu>,
    }

    impl crate::machine::ReactionSteps for HostShareSteps {
        fn reread_wall_clock(&mut self) -> impl Future<Output = ()> + '_ {
            core::future::ready(())
        }

        fn reset_host_share(&mut self, restore: HostShareRestore) -> impl Future<Output = ()> + '_ {
            self.client.after_machine_restore(restore)
        }

        fn revalidate_lease(&mut self) -> impl Future<Output = ()> + '_ {
            core::future::ready(())
        }

        fn resynchronise_balloon(&mut self) -> impl Future<Output = ()> + '_ {
            core::future::ready(())
        }
    }

    /// The 9p device was reset: its server has forgotten the session and
    /// every fid. Once the reset is recorded, the reactions task drops
    /// the session, so the next operation sends `Tversion` and `Tattach`
    /// and asks the server again rather than serving what was cached.
    #[test]
    fn a_reset_host_share_device_makes_the_next_operation_attach_again() {
        use core::task::{Context, Poll, Waker};

        use crate::machine::{MachineGeneration, ResetDevice, react_forever};

        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());
        block_on(client.stat_path_impl("/alpha")).expect("stat before");
        {
            let log = transport.log();
            assert_eq!((log.versions, log.attaches, log.getattrs), (1, 1, 1));
        }

        let generation = MachineGeneration::new(None);
        let mut context = Context::from_waker(Waker::noop());
        let mut reactions = pin!(react_forever(
            &generation,
            HostShareSteps {
                client: client.clone(),
            }
        ));
        assert_eq!(reactions.as_mut().poll(&mut context), Poll::Pending);
        assert!(
            client.inner.session.get().is_some(),
            "nothing is dropped before a reset"
        );

        generation.record_device_reset(ResetDevice::HostShare);
        assert_eq!(reactions.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            client.inner.session.get(),
            None,
            "the reactions task dropped the session"
        );

        block_on(client.stat_path_impl("/alpha")).expect("stat after");
        let log = transport.log();
        assert_eq!(
            (log.versions, log.attaches),
            (2, 2),
            "the next operation sends Tversion and Tattach"
        );
        assert_eq!(
            log.getattrs, 2,
            "attributes cached before the reset are not served"
        );
    }

    #[test]
    fn a_restore_that_kept_the_server_flushes_the_cache_and_clunks_the_parked_fid() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.read_file_impl("/alpha").await.expect("first read");
        });
        assert!(transport.log().clunked.is_empty());

        block_on(client.flush_cache());
        assert_eq!(
            transport.log().clunked.len(),
            1,
            "the pooled fid is still known to the server, so it is clunked"
        );

        block_on(async {
            client.stat_path_impl("/alpha").await.expect("stat after");
        });
        let log = transport.log();
        assert_eq!(
            (log.versions, log.attaches),
            (1, 1),
            "the session is the server's still and is kept"
        );
        assert_eq!(
            log.getattrs, 2,
            "the flushed attributes are asked for again"
        );
    }

    #[test]
    fn a_fid_handed_out_before_a_reset_cannot_be_confused_with_a_later_one() {
        let fids = HandleAllocator::new(4, P9_NOFID, "fid", "fids exhausted");
        let before = fids.allocate().expect("a fid").retain();
        let also_before = fids.allocate().expect("a fid").retain();
        assert!(fids.is_current(before));

        fids.reset();
        assert!(!fids.is_current(before));
        assert_eq!(fids.live_count(), 0);

        let after = fids.allocate().expect("a fid after the reset");
        assert!(
            ![before, also_before].contains(&after.value()),
            "a new fid never equals one an old operation still holds"
        );
        // An operation from before the reset returns its fid: ignored,
        // and the new fid stays live.
        fids.release(before);
        assert_eq!(fids.live_count(), 1);
        drop(after);
        assert_eq!(fids.live_count(), 0);
    }

    #[test]
    fn a_snapshot_hold_releases_the_root_fid_parks_operations_and_attaches_once_on_finish() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.stat_path("/alpha").await.expect("stat before");
            client.read_file("/alpha").await.expect("read before");
            let root_fid = client
                .inner
                .session
                .get()
                .expect("the session is attached")
                .root_fid;

            let clunked_before = transport.log().clunked.len();
            client.prepare_snapshot().await;
            {
                let log = transport.log();
                assert!(
                    log.clunked.contains(&root_fid),
                    "the root fid is clunked, which lifts the VMM's snapshot blocker"
                );
                assert_eq!(
                    log.clunked.len() - clunked_before,
                    2,
                    "the pooled read fid and the root fid"
                );
            }
            assert_eq!(client.inner.session.get(), None);
            assert_eq!(client.inner.fids.live_count(), 0, "no fid is left");

            // Two operations issued during the hold: neither fails and
            // neither reaches the server.
            let walks_before = transport.log().walks;
            let mut first = pin!(client.stat_path("/beta"));
            let mut second = pin!(client.read_dir("/", TEST_DIR_LIMIT));
            assert!(poll_once(first.as_mut()).await.is_none());
            assert!(poll_once(second.as_mut()).await.is_none());
            assert_eq!(transport.log().walks, walks_before);
            assert_eq!(transport.log().attaches, 1, "no attach during the hold");

            client.snapshot_finished();
            let (first, second) = zip(first, second).await;
            first.expect("the parked stat completes after the finish");
            second.expect("the parked listing completes after the finish");
        });

        let log = transport.log();
        assert_eq!(
            (log.versions, log.attaches),
            (2, 2),
            "the session is attached exactly once more, for both parked operations"
        );
        drop(log);
        assert_eq!(client.inner.fids.live_count(), 1, "only the new root fid");
    }

    #[test]
    fn a_machine_restore_ends_a_snapshot_hold() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.stat_path("/alpha").await.expect("stat before");
            client.prepare_snapshot().await;
            let mut parked = pin!(client.stat_path("/beta"));
            assert!(poll_once(parked.as_mut()).await.is_none());

            client
                .after_machine_restore(HostShareRestore::SessionLost)
                .await;
            parked.await.expect("the parked stat completes");
            // The source's `snapshot-finished` arriving after the restore
            // reaction must not disturb the new session.
            client.snapshot_finished();
            client.stat_path("/gamma").await.expect("stat after");
        });

        let log = transport.log();
        assert_eq!((log.versions, log.attaches), (2, 2));
    }

    #[test]
    fn preparing_a_snapshot_waits_for_the_operation_in_flight() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            let mut operation = pin!(client.stat_path("/alpha"));
            assert!(
                poll_once(operation.as_mut()).await.is_none(),
                "the fake server answers after a yield, so the operation is in flight"
            );

            let mut prepare = pin!(client.prepare_snapshot());
            let mut polls = 0;
            let outcome = loop {
                if let Some(outcome) = poll_once(operation.as_mut()).await {
                    break outcome;
                }
                assert!(
                    poll_once(prepare.as_mut()).await.is_none(),
                    "prepare finished while an operation was still in flight"
                );
                assert!(
                    !transport.log().clunked.contains(&0),
                    "the root fid is not clunked under an operation in flight"
                );
                polls += 1;
                assert!(polls < 100, "the operation never completed");
            };
            outcome.expect("the operation in flight completes, it is not failed");

            prepare.await;
        });

        let log = transport.log();
        assert_eq!(log.clunked.len(), 2, "the operation's fid and the root fid");
        drop(log);
        assert_eq!(client.inner.session.get(), None);
        assert_eq!(client.inner.fids.live_count(), 0);
    }

    #[test]
    fn a_path_the_host_does_not_have_is_remembered_as_missing() {
        let Fixture {
            client, transport, ..
        } = fixture(
            P9_REQUESTED_MSIZE,
            FakeFault {
                fail_type: Some(P9_TWALK),
                ..FakeFault::default()
            },
        );

        block_on(async {
            client
                .stat_path_impl("/missing")
                .await
                .expect_err("the host has no such path");
            client
                .stat_path_impl("/missing")
                .await
                .expect_err("and still has none");
        });

        assert_eq!(
            transport.log().walks,
            1,
            "the negative entry answers the second stat without a walk"
        );
        assert_eq!(client.cache_stats().negative_hits, 1);
    }

    #[test]
    fn reading_one_file_twice_reuses_its_fid() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            let first = client.read_file_impl("/alpha").await.expect("first read");
            let second = client.read_file_impl("/alpha").await.expect("second read");
            assert_eq!(first.len(), FILE_CONTENT_LEN);
            assert_eq!(second, first);
        });

        let log = transport.log();
        assert_eq!(log.walks, 1, "the second read reuses the parked fid");
        assert_eq!(log.opens, 1, "a parked fid is already open");
        assert!(
            log.clunked.is_empty(),
            "the fid stays parked for the next read instead of being clunked"
        );
        drop(log);
        assert_eq!(client.cache_stats().fid_hits, 1);
        assert_eq!(client.cache_stats().fid_misses, 1);
    }

    #[test]
    fn a_parked_fid_past_its_ttl_is_clunked_before_a_fresh_one_is_opened() {
        let Fixture {
            client,
            transport,
            clock,
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.read_file_impl("/alpha").await.expect("first read");
            clock.advance(super::super::cache::CACHE_TTL_NANOS);
            client.read_file_impl("/alpha").await.expect("second read");
        });

        let log = transport.log();
        assert_eq!(log.opens, 2, "the stale fid cannot be read from again");
        assert_eq!(
            log.clunked.len(),
            1,
            "the fid the pool gave up is released on the server too"
        );
        drop(log);
        assert_eq!(
            client.inner.fids.live_count(),
            2,
            "the session root fid, plus the fid the second read parked"
        );
    }

    #[test]
    fn a_directory_listing_is_served_from_the_cache() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client
                .read_dir_impl("/alpha", TEST_DIR_LIMIT)
                .await
                .expect("first listing");
            client
                .read_dir_impl("/alpha", TEST_DIR_LIMIT)
                .await
                .expect("cached listing");
        });

        assert_eq!(transport.log().readdirs, 1);
        assert_eq!(client.cache_stats().directory_hits, 1);
    }

    #[test]
    fn a_repeated_readdir_batch_is_a_protocol_error() {
        let Fixture {
            client, transport, ..
        } = fixture(
            P9_REQUESTED_MSIZE,
            FakeFault {
                readdir: ReaddirFault::RepeatBatch,
                ..FakeFault::default()
            },
        );

        let error = block_on(client.read_dir_impl("/alpha", TEST_DIR_LIMIT))
            .expect_err("a server that repeats a batch must not loop the client");

        assert!(
            matches!(error, HostFsError::Protocol(message) if message.contains("strictly increase")),
            "{error:?}"
        );
        assert_eq!(
            transport.log().readdirs,
            2,
            "the second batch repeats the first one's offset and ends the listing"
        );
    }

    #[test]
    fn a_listing_past_the_callers_limit_is_an_error() {
        let limit = HostDirLimit {
            max_entries: 10,
            max_name_bytes: 1 << 20,
        };
        let client = client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                readdir: ReaddirFault::Endless,
                ..FakeFault::default()
            },
        );

        let error = block_on(client.read_dir_impl("/alpha", limit))
            .expect_err("an endless directory must hit the entry bound");

        assert!(
            matches!(
                error,
                HostFsError::DirectoryTooLarge {
                    max_entries: 10,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_listing_past_the_callers_name_byte_limit_is_an_error() {
        let limit = HostDirLimit {
            max_entries: 1 << 16,
            max_name_bytes: 64,
        };
        let client = client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                readdir: ReaddirFault::Endless,
                ..FakeFault::default()
            },
        );

        let error = block_on(client.read_dir_impl("/alpha", limit))
            .expect_err("an endless directory must hit the byte bound");

        assert!(
            matches!(
                error,
                HostFsError::DirectoryTooLarge {
                    max_name_bytes: 64,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn creating_a_file_invalidates_the_parent_listing() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client
                .read_dir_impl("/alpha", TEST_DIR_LIMIT)
                .await
                .expect("first listing");
            client
                .create_file_impl("/alpha/beta")
                .await
                .expect("create the file");
            client
                .read_dir_impl("/alpha", TEST_DIR_LIMIT)
                .await
                .expect("fresh listing");
        });

        assert_eq!(
            transport.log().readdirs,
            2,
            "the new name means the cached listing no longer describes the directory"
        );
    }

    #[test]
    fn writing_a_file_invalidates_its_cached_attributes() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.stat_path_impl("/alpha/beta").await.expect("stat");
            client
                .write_file_impl("/alpha/beta", 0, b"payload")
                .await
                .expect("write");
            client.stat_path_impl("/alpha/beta").await.expect("restat");
        });

        assert_eq!(
            transport.log().getattrs,
            2,
            "the write changed the size the cached attributes reported"
        );
    }

    #[test]
    fn removing_a_file_clunks_the_fid_the_pool_parked_for_it() {
        let Fixture {
            client, transport, ..
        } = fixture(P9_REQUESTED_MSIZE, FakeFault::default());

        let parked = block_on(async {
            client.read_file_impl("/alpha/beta").await.expect("read");
            let parked = client.inner.fids.live_count();
            client
                .remove_impl("/alpha/beta", false)
                .await
                .expect("remove");
            parked
        });

        assert_eq!(
            parked, 2,
            "the read parked a fid alongside the session root"
        );
        assert_eq!(
            client.inner.fids.live_count(),
            1,
            "removing the path releases the fid the pool held open on it"
        );
        assert_eq!(
            transport.log().clunked.len(),
            2,
            "the write-side fid and the parked read fid are both released"
        );
    }

    #[test]
    fn a_server_that_grants_an_unusable_msize_is_rejected() {
        let client = client(P9_MIN_MSIZE - 1, FakeFault::default());

        let error = block_on(client.stat_path_impl("/alpha")).expect_err("the msize is too small");

        assert!(matches!(error, HostFsError::Protocol(_)), "{error:?}");
    }

    /// A client built on a live [`crate::ProfileSink`], so a test can
    /// read back what the instrumented paths recorded.
    fn profiled_client(
        msize: u32,
        fault: FakeFault,
    ) -> (
        HostFsClient<FakeTransport, ManualClockCpu>,
        crate::ProfileSink,
    ) {
        let profiles = crate::test_support::test_profile_sink();
        profiles.set_enabled(true);
        let transport = FakeTransport::new(msize, fault);
        let client = HostFsClient::new(transport, ManualClockCpu::new(), profiles.clone());
        (client, profiles)
    }

    /// The `kernel;hostfs;*` samples `profiles` holds, keyed by counter
    /// name.
    fn hostfs_samples(
        profiles: &crate::ProfileSink,
    ) -> alloc::collections::BTreeMap<String, crate::PerfMetricSample> {
        profiles
            .perf_metrics()
            .lock()
            .recent(
                &crate::PerfMetricFilter {
                    name_prefixes: alloc::vec!["kernel;hostfs;".to_owned()],
                },
                u32::MAX,
            )
            .into_iter()
            .map(|sample| (sample.name.clone(), sample))
            .collect()
    }

    #[test]
    fn message_types_name_their_wire_byte_reply_and_phase() {
        for message in [
            P9Message::Version,
            P9Message::Attach,
            P9Message::Walk,
            P9Message::Read,
            P9Message::Write,
            P9Message::Clunk,
            P9Message::Lopen,
            P9Message::Lcreate,
            P9Message::Symlink,
            P9Message::Readlink,
            P9Message::Getattr,
            P9Message::Setattr,
            P9Message::Readdir,
            P9Message::Fsync,
            P9Message::Link,
            P9Message::Mkdir,
            P9Message::Renameat,
            P9Message::Unlinkat,
        ] {
            assert_eq!(message.reply_wire(), message.wire() + 1);
            assert!(
                message.phase().starts_with("msg-"),
                "{:?} has no msg-* phase name",
                message.wire()
            );
        }
        assert_eq!(P9Message::Version.phase(), "msg-version");
        assert_eq!(P9Message::Attach.phase(), "msg-attach");
        assert_eq!(P9Message::Walk.phase(), "msg-walk");
        assert_eq!(P9Message::Read.phase(), "msg-read");
        assert_eq!(P9Message::Lopen.phase(), "msg-lopen");
        assert_eq!(P9Message::Getattr.phase(), "msg-getattr");
        assert_eq!(P9Message::Readdir.phase(), "msg-readdir");
        assert_eq!(P9Message::Clunk.phase(), "msg-clunk");
    }

    #[test]
    fn a_profiled_client_records_operation_message_and_cache_phases() {
        let (client, profiles) = profiled_client(P9_REQUESTED_MSIZE, FakeFault::default());

        block_on(async {
            client.stat_path("/alpha").await.expect("first stat");
            client.stat_path("/alpha").await.expect("cached stat");
            client.read_file("/alpha").await.expect("read");
        });

        let samples = hostfs_samples(&profiles);
        let metric = |name: &str| {
            samples
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing from {samples:?}"))
        };
        // Two stats and one read, each measured end to end.
        assert_eq!(metric("kernel;hostfs;op-stat").count, 2);
        assert_eq!(metric("kernel;hostfs;op-read-file").count, 1);
        assert_eq!(
            metric("kernel;hostfs;op-read-file").total_bytes,
            u64::try_from(FILE_CONTENT_LEN).expect("file length fits u64"),
            "op-read-file carries the payload bytes the read moved"
        );
        // The handshake runs once for the session; the first stat and
        // the read's fid acquisition each walk, the second stat is a
        // cache hit and walks nothing.
        for (name, count) in [
            ("kernel;hostfs;msg-version", 1),
            ("kernel;hostfs;msg-attach", 1),
            ("kernel;hostfs;msg-walk", 2),
            ("kernel;hostfs;msg-getattr", 1),
            ("kernel;hostfs;msg-lopen", 1),
            ("kernel;hostfs;msg-read", 1),
            ("kernel;hostfs;msg-clunk", 1),
        ] {
            assert_eq!(metric(name).count, count, "{name}");
            assert_eq!(
                metric(name).total_events,
                count,
                "{name} counts one event per message"
            );
        }
        // The first stat missed the attribute table; the second stat
        // and the read's size probe hit it.
        assert_eq!(metric("kernel;hostfs;cache-miss").total_events, 1);
        assert_eq!(metric("kernel;hostfs;cache-hit").total_events, 2);
        assert_eq!(
            metric("kernel;hostfs;cache-hit").total_nanos,
            0,
            "cache counters carry a count and no interval"
        );
    }

    #[test]
    fn an_rlerror_reply_still_records_its_round_trip() {
        let (client, profiles) = profiled_client(
            P9_REQUESTED_MSIZE,
            FakeFault {
                fail_type: Some(P9_TLOPEN),
                ..FakeFault::default()
            },
        );

        block_on(client.write_file("/alpha", 0, b"payload"))
            .expect_err("the server refuses to open the file");

        let samples = hostfs_samples(&profiles);
        let metric = |name: &str| {
            samples
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing from {samples:?}"))
        };
        assert_eq!(
            metric("kernel;hostfs;msg-lopen").count,
            1,
            "a refused open is still one completed exchange"
        );
        assert_eq!(
            metric("kernel;hostfs;op-write").count,
            1,
            "the failed operation is counted too"
        );
    }
}
