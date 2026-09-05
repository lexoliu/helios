//! Wasmtime trait implementations for `ComponentStoreData`.
//!
//! These trait impls bridge kernel-owned store state to Wasmtime's runtime
//! requirements. They live in the adapter module so that `component_runtime.rs`
//! stays free of Wasmtime imports.

extern crate alloc;

use alloc::boxed::Box;

use helios_hal::cpu::Cpu;
use wasmtime::component::ResourceTable;
use wasmtime::{CallHook, ResourceLimiter};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{InputStream, OutputStream, StreamError, StreamResult};

use crate::io::{ByteReader, ByteWriter};
use crate::{
    ComponentRuntimeState, ComponentStoreData, KernelHeapHeadroom, MemoryPool, OomKillOutcome,
    ProgramOutOfMemory, allow_instance_resource_growth, heap_stats, monotonic_nanos,
    user_heap_stats,
};

impl<CpuImpl, RuntimeStateImpl, FileSystem> ResourceLimiter
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem, ResourceTable>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|maximum| desired > maximum) {
            return Err(ProgramOutOfMemory {
                requested_bytes: desired,
                available_bytes: 0,
                pool_bytes: 0,
                reserved_bytes: 0,
            }
            .into());
        }

        let growth = desired.saturating_sub(current);

        if let Some(error) = self.try_satisfy_or_kill(desired, growth) {
            return Err(error);
        }

        Ok(allow_instance_resource_growth(
            self.instance(),
            desired,
            maximum,
        ))
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        Err(error)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(maximum.is_none_or(|maximum| desired <= maximum))
    }
}

impl<CpuImpl, RuntimeStateImpl, FileSystem>
    ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem, ResourceTable>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    /// Decide whether `growth` extra bytes of user memory can be granted.
    ///
    /// Two pools have to answer, and they are asked for different
    /// amounts. The user pool is asked for the growth itself: that is
    /// where the guest's pages come from. The kernel heap is asked only
    /// for what the growth costs *it* — the page tables and reservation
    /// records that address the new pages
    /// ([`crate::user_mapping_kernel_heap_bytes`]), around a
    /// five-hundredth of the growth. Charging the growth itself against
    /// the kernel heap, as this did, refused grows the kernel heap was
    /// never asked to fund.
    ///
    /// On insufficient user heap or kernel reserve breach, the OOM
    /// killer is asked to mark the largest non-self victim *of the pool
    /// that ran out* for termination (so the next host-call boundary on
    /// that instance traps with [`crate::InstanceKilled`] and frees its
    /// memory). The growing request itself still returns
    /// `ProgramOutOfMemory` — reclamation happens asynchronously and the
    /// requester retries on the next allocation attempt or surfaces the
    /// error to its caller.
    fn try_satisfy_or_kill(&self, desired: usize, growth: usize) -> Option<wasmtime::Error> {
        let user_heap = user_heap_stats();
        let user_available = user_heap.available_bytes();
        if user_available < growth {
            self.request_oom_kill_for_growth(MemoryPool::User, growth);
            return Some(
                ProgramOutOfMemory {
                    requested_bytes: desired,
                    available_bytes: user_available,
                    pool_bytes: user_heap.total_bytes,
                    reserved_bytes: 0,
                }
                .into(),
            );
        }

        let heap = heap_stats();
        let headroom = KernelHeapHeadroom::of(heap);
        if let Some(shortfall) = headroom.growth_shortfall_bytes(growth) {
            self.request_oom_kill_for_growth(MemoryPool::Kernel, shortfall);
            // Every number in this refusal describes the kernel heap,
            // including the request: reporting the guest's `desired`
            // here alongside kernel-heap availability is what made a
            // refusal on this branch read as a user-pool refusal.
            return Some(
                ProgramOutOfMemory {
                    requested_bytes: shortfall,
                    available_bytes: headroom.available_bytes,
                    pool_bytes: heap.total_bytes,
                    reserved_bytes: headroom.reserve_bytes,
                }
                .into(),
            );
        }

        None
    }

    /// Ask the OOM killer to cover `requested_bytes` of `pool`.
    ///
    /// The killer answers against its condemned-memory ledger, so a
    /// shortfall that an in-flight kill already covers condemns nothing
    /// further: `memory_growing` is a synchronous Wasmtime callback and
    /// cannot wait for the reclaim, so the requester takes its typed
    /// `ProgramOutOfMemory` failure either way and retries when the
    /// guest next asks for the memory. Logged at warn level so
    /// post-mortem analysis can trace which instance was sacrificed for
    /// which grow request, and which requests were absorbed by a kill
    /// that had already happened.
    fn request_oom_kill_for_growth(&self, pool: MemoryPool, requested_bytes: usize) {
        // VIRTIO_BALLOON_F_DEFLATE_ON_OOM: memory the host is holding is
        // reclaimed before memory a program is using. The balloon task
        // does the deflating, so this only asks; the requester still
        // takes this grow failure and retries.
        if let Some(balloon) = self.runtime_state.memory_balloon() {
            balloon.request_deflate();
        }
        let requester = self.instance().id();
        let decision = self.instance_registry.condemn_for_oom(
            requester,
            pool,
            requested_bytes as u64,
            monotonic_nanos(&self.cpu),
        );
        let condemned = decision.condemned;
        match decision.outcome {
            OomKillOutcome::Condemned(victim) => tracing::warn!(
                target: "helios_kernel::oom",
                requester = ?requester,
                pool = ?pool,
                requested_bytes,
                victim_id = ?victim.id,
                victim_name = %victim.name,
                victim_memory_bytes = victim.memory_bytes,
                victim_kernel_bytes = victim.kernel_bytes,
                victim_policy = ?victim.policy,
                score = victim.score,
                condemned_pending_bytes = condemned.pending_bytes,
                condemned_stale_bytes = condemned.stale_bytes,
                "OOM killer condemned victim to free memory"
            ),
            OomKillOutcome::AwaitingReclaim => tracing::debug!(
                target: "helios_kernel::oom",
                requester = ?requester,
                pool = ?pool,
                requested_bytes,
                condemned_pending_bytes = condemned.pending_bytes,
                condemned_stale_bytes = condemned.stale_bytes,
                "memory already condemned covers this grow — requester retries instead of \
                 condemning another instance"
            ),
            OomKillOutcome::NoVictim => tracing::warn!(
                target: "helios_kernel::oom",
                requester = ?requester,
                pool = ?pool,
                requested_bytes,
                condemned_pending_bytes = condemned.pending_bytes,
                condemned_stale_bytes = condemned.stale_bytes,
                "OOM killer found no eligible victim — requester takes the grow failure"
            ),
        }
    }
}

impl<CpuImpl, RuntimeStateImpl, FileSystem> IoView
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem, ResourceTable>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeStateImpl> Pollable for crate::DeadlinePollable<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    async fn ready(&mut self) {
        crate::DeadlinePollable::ready(self).await;
    }
}

/// WASI P2 adapter: exposes a kernel `ByteReader` as an `InputStream`.
///
/// Wasmtime drives `Pollable::ready` and `InputStream::read` through `&mut
/// self`, so the carry buffer is owned outright rather than wrapped in
/// `Arc<Mutex<_>>`.
pub struct ChannelInputStream {
    reader: ByteReader,
    carry: Bytes,
}

impl ChannelInputStream {
    pub fn new(reader: ByteReader) -> Self {
        Self {
            reader,
            carry: Bytes::new(),
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for ChannelInputStream {
    async fn ready(&mut self) {
        if !self.carry.is_empty() {
            return;
        }
        if let Some(bytes) = self.reader.read().await {
            self.carry = bytes;
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for ChannelInputStream {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        if !self.carry.is_empty() {
            let take = self.carry.len().min(size);
            return Ok(self.carry.split_to(take));
        }
        match self.reader.try_read() {
            crate::io::TryRead::Ready(mut bytes) => {
                if bytes.len() > size {
                    let taken = bytes.split_to(size);
                    self.carry = bytes;
                    return Ok(taken);
                }
                Ok(bytes)
            }
            crate::io::TryRead::Pending => Ok(Bytes::new()),
            crate::io::TryRead::Eof => Err(StreamError::Closed),
        }
    }
}

/// Bytes a p2 `check-write` permit is worth while the channel has room.
const P2_CHANNEL_WRITE_PERMIT_BYTES: usize = 64 * 1024;

/// WASI P2 adapter: exposes a kernel `ByteWriter` as an `OutputStream`.
///
/// The p2 contract is permit-driven and `write` must never block, so a
/// full channel is handled the same way the host-file output stream
/// handles a 9p round trip: `check_write` withholds the permit, one batch
/// is parked in `pending`, and `Pollable::ready` completes it once the
/// reader has drained. No byte is dropped and a transiently full channel
/// is never reported as an error.
pub struct ChannelOutputStream {
    writer: ByteWriter,
    pending: Option<Bytes>,
}

impl ChannelOutputStream {
    pub fn new(writer: ByteWriter) -> Self {
        Self {
            writer,
            pending: None,
        }
    }

    /// Hand the parked batch to the channel, waiting for room.
    async fn flush_pending(&mut self) {
        let Some(bytes) = self.pending.take() else {
            // Nothing parked: report ready as soon as the channel could
            // take a batch, so the guest's next `check-write` is non-zero.
            self.writer.writable().await;
            return;
        };
        // A vanished reader is surfaced by the next `check_write`/`write`
        // as `StreamError::Closed`; there is nothing left to deliver.
        let _: Result<(), crate::ClosedPeer> = self.writer.write(bytes).await;
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for ChannelOutputStream {
    async fn ready(&mut self) {
        self.flush_pending().await;
    }
}

#[wasmtime_wasi_io::async_trait]
impl OutputStream for ChannelOutputStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if self.writer.is_reader_closed() {
            return Err(StreamError::Closed);
        }
        if self.pending.is_some() {
            return Err(StreamError::trap(
                "channel output stream write exceeded its check-write permit",
            ));
        }
        match self.writer.try_write(bytes) {
            crate::TryWrite::Written => Ok(()),
            // Park the batch rather than dropping it or erroring: `ready`
            // completes it and `check_write` withholds the next permit
            // until then.
            crate::TryWrite::Full(bytes) => {
                self.pending = Some(bytes);
                Ok(())
            }
            crate::TryWrite::Closed => Err(StreamError::Closed),
        }
    }

    fn flush(&mut self) -> StreamResult<()> {
        // A parked batch is drained by `ready`, which `check_write` pends
        // on, so there is nothing to push here.
        if self.writer.is_reader_closed() {
            return Err(StreamError::Closed);
        }
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self.writer.is_reader_closed() {
            return Err(StreamError::Closed);
        }
        if self.pending.is_some() || !self.writer.is_writable() {
            return Ok(0);
        }
        Ok(P2_CHANNEL_WRITE_PERMIT_BYTES)
    }
}

/// WASI P2 stdout/stderr adapter: writes through either a child-IO byte
/// channel or the serial debug port depending on the routing decided when
/// the store was constructed.
pub enum StdioOutputStream {
    /// Bytes flow to a parent-facing byte channel.
    Child(ChannelOutputStream),
    /// Bytes flow to the serial debug port.
    Serial(fn(&[u8])),
    /// Bytes are recorded as observer trace events (no-op here because
    /// serial debug does not model trace output for P2 programs).
    Trace,
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for StdioOutputStream {
    async fn ready(&mut self) {
        match self {
            // The child channel is bounded, so readiness is real work:
            // complete a parked batch and wait for room.
            StdioOutputStream::Child(inner) => inner.ready().await,
            StdioOutputStream::Serial(_) | StdioOutputStream::Trace => {}
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl OutputStream for StdioOutputStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        match self {
            StdioOutputStream::Child(inner) => inner.write(bytes),
            StdioOutputStream::Serial(writer) => {
                writer(bytes.as_ref());
                Ok(())
            }
            StdioOutputStream::Trace => Ok(()),
        }
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        match self {
            StdioOutputStream::Child(inner) => inner.check_write(),
            StdioOutputStream::Serial(_) | StdioOutputStream::Trace => Ok(64 * 1024),
        }
    }
}

/// Hook adapter that translates `wasmtime::CallHook` into kernel instance
/// execution transitions.
pub(crate) fn translate_call_hook(hook: CallHook) -> crate::InstanceExecutionTransition {
    match hook {
        CallHook::CallingWasm | CallHook::ReturningFromHost => {
            crate::InstanceExecutionTransition::Resume
        }
        CallHook::ReturningFromWasm | CallHook::CallingHost => {
            crate::InstanceExecutionTransition::Pause
        }
    }
}
