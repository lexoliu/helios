//! Wasmtime trait implementations for `ComponentStoreData`.
//!
//! These trait impls bridge kernel-owned store state to Wasmtime's runtime
//! requirements. They live in the adapter module so that `component_runtime.rs`
//! stays free of Wasmtime imports.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use spin::Mutex;
use wasmtime::component::ResourceTable;
use wasmtime::{CallHook, ResourceLimiter};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{InputStream, OutputStream, StreamError, StreamResult};

use crate::child_io::{ByteReader, ByteWriter};
use crate::{
    ComponentRuntimeState, ComponentStoreData, ProgramOutOfMemory, allow_instance_resource_growth,
    heap_stats, user_heap_stats, user_memory_kernel_reserve_bytes,
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
        let user_heap = user_heap_stats();
        let user_available = user_heap.available_bytes();
        if user_available < growth {
            return Err(ProgramOutOfMemory {
                requested_bytes: desired,
                available_bytes: user_available,
                reserved_bytes: 0,
            }
            .into());
        }

        let heap = heap_stats();
        let reserve = user_memory_kernel_reserve_bytes(heap.total_bytes);
        let available = heap.available_bytes();
        if available.saturating_sub(growth) < reserve {
            return Err(ProgramOutOfMemory {
                requested_bytes: desired,
                available_bytes: available,
                reserved_bytes: reserve,
            }
            .into());
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
pub struct ChannelInputStream {
    reader: ByteReader,
    carry: Arc<Mutex<Vec<u8>>>,
}

impl ChannelInputStream {
    pub fn new(reader: ByteReader) -> Self {
        Self {
            reader,
            carry: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for ChannelInputStream {
    async fn ready(&mut self) {
        if !self.carry.lock().is_empty() {
            return;
        }
        if let Some(bytes) = self.reader.read().await {
            self.carry.lock().extend_from_slice(&bytes);
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for ChannelInputStream {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        let mut carry = self.carry.lock();
        if !carry.is_empty() {
            let take = carry.len().min(size);
            let remainder = carry.split_off(take);
            let taken = core::mem::replace(&mut *carry, remainder);
            return Ok(Bytes::from(taken));
        }
        match self.reader.try_read() {
            crate::child_io::TryRead::Ready(mut bytes) => {
                if bytes.len() > size {
                    let tail = bytes.split_off(size);
                    *carry = tail;
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
