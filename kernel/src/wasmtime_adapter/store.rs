//! Wasmtime trait implementations for `ComponentStoreData`.
//!
//! These trait impls bridge kernel-owned store state to Wasmtime's runtime
//! requirements. They live in the adapter module so that `component_runtime.rs`
//! stays free of Wasmtime imports.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use wasmtime::component::ResourceTable;
use wasmtime::{CallHook, ResourceLimiter};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{InputStream, OutputStream, StreamError, StreamResult};

use crate::child_io::{ByteReader, ByteWriter};
use crate::{
    ComponentRuntimeState, ComponentStoreData, KillReason, ProgramOutOfMemory,
    allow_instance_resource_growth, heap_stats, user_heap_stats,
    user_memory_kernel_reserve_bytes,
};

impl<CpuImpl, RuntimeStateImpl, FileSystem> ResourceLimiter
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
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

impl<CpuImpl, RuntimeStateImpl, FileSystem> ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    /// Decide whether `growth` extra bytes of user memory can be granted.
    ///
    /// On insufficient user heap or kernel reserve breach, the OOM
    /// killer is asked to mark the largest non-self victim for
    /// termination (so the next host-call boundary on that instance
    /// traps with [`crate::InstanceKilled`] and frees its memory). The
    /// growing request itself still returns `ProgramOutOfMemory` —
    /// reclamation happens asynchronously and the requester retries on
    /// the next allocation attempt or surfaces the error to its caller.
    fn try_satisfy_or_kill(&self, desired: usize, growth: usize) -> Option<wasmtime::Error> {
        let user_heap = user_heap_stats();
        let user_available = user_heap.available_bytes();
        if user_available < growth {
            self.request_oom_kill_for_growth(growth);
            return Some(
                ProgramOutOfMemory {
                    requested_bytes: desired,
                    available_bytes: user_available,
                    reserved_bytes: 0,
                }
                .into(),
            );
        }

        let heap = heap_stats();
        let reserve = user_memory_kernel_reserve_bytes(heap.total_bytes);
        let available = heap.available_bytes();
        if available.saturating_sub(growth) < reserve {
            self.request_oom_kill_for_growth(growth);
            return Some(
                ProgramOutOfMemory {
                    requested_bytes: desired,
                    available_bytes: available,
                    reserved_bytes: reserve,
                }
                .into(),
            );
        }

        None
    }

    /// Pick the highest-scoring OOM victim that is not the requesting
    /// instance and flag it for kill. Logged at warn level so post-mortem
    /// analysis can trace which instance was sacrificed for which grow
    /// request.
    fn request_oom_kill_for_growth(&self, requested_bytes: usize) {
        let registry = &self.instance_registry;
        let requester = self.instance().id();
        let mut victim = registry.pick_oom_victim();
        // Avoid suiciding: if the highest-scoring victim is the requester
        // itself, the OOM killer hands a grow failure back to that
        // instance instead of marking the killer to terminate. Other
        // instances may still be picked on subsequent grow attempts as
        // they accumulate memory.
        if let Some(candidate) = &victim
            && candidate.id == requester
        {
            victim = None;
        }
        let Some(victim) = victim else {
            tracing::warn!(
                target: "helios_kernel::oom",
                requester = ?requester,
                requested_bytes,
                "OOM killer found no eligible victim — requester takes the grow failure"
            );
            return;
        };
        let killed = registry.request_kill(victim.id, KillReason::OutOfMemory);
        tracing::warn!(
            target: "helios_kernel::oom",
            requester = ?requester,
            requested_bytes,
            victim_id = ?victim.id,
            victim_name = %victim.name,
            victim_memory_bytes = victim.memory_bytes,
            victim_restart_cost = victim.restart_cost,
            score = victim.score,
            killed,
            "OOM killer condemned victim to free user memory"
        );
    }
}

impl<CpuImpl, RuntimeStateImpl, FileSystem> IoView
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
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
        while self.uptime_nanos() < self.deadline_nanos() {
            crate::yield_now().await;
        }
    }
}

/// WASI P2 adapter: exposes a kernel `ByteReader` as an `InputStream`.
///
/// Wasmtime drives `Pollable::ready` and `InputStream::read` through `&mut
/// self`, so the carry buffer is owned outright rather than wrapped in
/// `Arc<Mutex<_>>`.
pub struct ChannelInputStream {
    reader: ByteReader,
    carry: Vec<u8>,
}

impl ChannelInputStream {
    pub fn new(reader: ByteReader) -> Self {
        Self {
            reader,
            carry: Vec::new(),
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
            self.carry.extend_from_slice(&bytes);
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for ChannelInputStream {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        if !self.carry.is_empty() {
            let take = self.carry.len().min(size);
            let remainder = self.carry.split_off(take);
            let taken = core::mem::replace(&mut self.carry, remainder);
            return Ok(Bytes::from(taken));
        }
        match self.reader.try_read() {
            crate::child_io::TryRead::Ready(mut bytes) => {
                if bytes.len() > size {
                    let tail = bytes.split_off(size);
                    self.carry = tail;
                }
                Ok(Bytes::from(bytes))
            }
            crate::child_io::TryRead::Pending => Ok(Bytes::new()),
            crate::child_io::TryRead::Eof => Err(StreamError::Closed),
        }
    }
}

/// WASI P2 adapter: exposes a kernel `ByteWriter` as an `OutputStream`.
pub struct ChannelOutputStream {
    writer: ByteWriter,
}

impl ChannelOutputStream {
    pub fn new(writer: ByteWriter) -> Self {
        Self { writer }
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for ChannelOutputStream {
    async fn ready(&mut self) {
        // Always ready to accept writes; back-pressure is not modelled on
        // the unbounded child IO channel.
    }
}

#[wasmtime_wasi_io::async_trait]
impl OutputStream for ChannelOutputStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if self.writer.is_reader_closed() {
            return Err(StreamError::Closed);
        }
        self.writer
            .write(bytes.to_vec())
            .map_err(|_| StreamError::Closed)
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self.writer.is_reader_closed() {
            return Err(StreamError::Closed);
        }
        Ok(64 * 1024)
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
    async fn ready(&mut self) {}
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
