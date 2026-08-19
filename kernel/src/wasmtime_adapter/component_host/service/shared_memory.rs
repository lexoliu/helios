use super::*;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct SharedMemorySpec {
    pub(super) initial_pages: u32,
    pub(super) maximum_pages: u32,
}

impl SharedMemorySpec {
    pub(super) fn byte_size(self) -> usize {
        usize::try_from(self.initial_pages)
            .expect("shared-memory page count must fit usize")
            .checked_mul(WASM_PAGE_SIZE)
            .expect("shared-memory byte size overflow")
    }

    pub(super) fn memory_type(self) -> MemoryType {
        MemoryType::shared(self.initial_pages, self.maximum_pages)
    }
}

pub(super) struct SharedMemoryPool {
    pub(super) budget_bytes: usize,
    pub(super) resident_bytes: usize,
    pub(super) buckets: HashMap<SharedMemorySpec, Vec<SharedMemory>>,
}

impl SharedMemoryPool {
    pub(super) fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            buckets: HashMap::new(),
        }
    }

    /// Hands out a pre-zeroed pooled memory, or a fresh one. Pooled
    /// entries were scrubbed off the spawn path by the recycle task, so
    /// this never zeroes memory on the process-start critical path.
    pub(super) fn acquire(
        &mut self,
        engine: &wasmtime::Engine,
        spec: SharedMemorySpec,
    ) -> Result<SharedMemory, ProgramExecError> {
        if let Some(memory) = self.buckets.get_mut(&spec).and_then(|bucket| bucket.pop()) {
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(spec.byte_size())
                .expect("shared-memory pool byte accounting underflow");
            return Ok(memory);
        }

        SharedMemory::new(engine, spec.memory_type()).map_err(map_program_runtime_error)
    }

    /// Claims budget for a memory about to be scrubbed and re-pooled.
    /// The bytes count as resident from this point so a burst of exits
    /// cannot overshoot the pool budget while scrubs are in flight.
    pub(super) fn reserve_for_recycle(&mut self, spec: SharedMemorySpec, memory: &SharedMemory) -> bool {
        if memory.size() != u64::from(spec.initial_pages) {
            return false;
        }
        let bytes = spec.byte_size();
        if self.resident_bytes.saturating_add(bytes) > self.budget_bytes {
            return false;
        }
        self.resident_bytes = self
            .resident_bytes
            .checked_add(bytes)
            .expect("shared-memory pool byte accounting overflow");
        true
    }

    /// Returns a scrubbed memory to its bucket. Budget was claimed by
    /// `reserve_for_recycle`.
    pub(super) fn finish_recycle(&mut self, spec: SharedMemorySpec, memory: SharedMemory) {
        self.buckets.entry(spec).or_default().push(memory);
    }
}

/// Recycles an exited guest's shared memory through a background scrub
/// task: zeroing a multi-megabyte memory is the dominant cost of
/// re-pooling, and doing it here would land it on the child's exit path
/// (which `proc_join` waits on). The scrub overlaps with other guests
/// on other processors and yields between chunks.
pub(super) fn spawn_scrubbed_recycle<CpuImpl>(
    spawner: &crate::Spawner<CpuImpl>,
    pool: Arc<Mutex<SharedMemoryPool>>,
    spec: SharedMemorySpec,
    memory: SharedMemory,
) where
    CpuImpl: Cpu + Clone,
{
    if !pool.lock().reserve_for_recycle(spec, &memory) {
        return;
    }
    spawner.spawn_detached(async move {
        scrub_shared_memory(&memory).await;
        pool.lock().finish_recycle(spec, memory);
    });
}

/// Bytes zeroed between cooperative yields while scrubbing.
const SCRUB_CHUNK_BYTES: usize = 512 * 1024;

pub(super) async fn scrub_shared_memory(memory: &SharedMemory) {
    let len = memory.data_size();
    let mut offset = 0;
    while offset < len {
        let chunk = SCRUB_CHUNK_BYTES.min(len - offset);
        // Re-derive the pointer each chunk: holding a raw pointer across
        // the yield would make the scrub future non-Send.
        let base = memory.data().as_ptr().cast::<u8>() as *mut u8;
        // SAFETY: the previous Store/Instance holders have been dropped
        // and the memory is owned by the scrub task until it re-enters
        // the pool, so no guest can observe the partial zeroing.
        unsafe {
            core::ptr::write_bytes(base.add(offset), 0, chunk);
        }
        offset += chunk;
        if offset < len {
            crate::exec::yield_now().await;
        }
    }
}

pub(super) fn imported_shared_memory_with_declared_maximum(
    engine: &wasmtime::Engine,
    module: &Module,
) -> Result<Option<SharedMemory>, ProgramExecError> {
    imported_shared_memory(engine, module, None)
}

pub(super) fn imported_shared_memory_spec_with_user_budget(
    module: &Module,
) -> Result<Option<SharedMemorySpec>, ProgramExecError> {
    let available_pages = user_heap_stats().available_bytes() / WASM_PAGE_SIZE;
    let available_pages = u32::try_from(available_pages).unwrap_or(u32::MAX);
    let budget_pages = available_pages.min(PROGRAM_SHARED_MEMORY_MAX_PAGES);
    imported_shared_memory_spec(module, Some(budget_pages))
}

pub(super) fn imported_shared_memory(
    engine: &wasmtime::Engine,
    module: &Module,
    maximum_pages_budget: Option<u32>,
) -> Result<Option<SharedMemory>, ProgramExecError> {
    let Some(spec) = imported_shared_memory_spec(module, maximum_pages_budget)? else {
        return Ok(None);
    };
    SharedMemory::new(engine, spec.memory_type())
        .map(Some)
        .map_err(map_program_runtime_error)
}

pub(super) fn imported_shared_memory_spec(
    module: &Module,
    maximum_pages_budget: Option<u32>,
) -> Result<Option<SharedMemorySpec>, ProgramExecError> {
    let mut memory_type = None;
    for import in module.imports() {
        if import.module() == "env" && import.name() == "memory" {
            memory_type = import.ty().memory().cloned();
            break;
        }
    }
    let Some(memory_type) = memory_type else {
        return Ok(None);
    };
    if !memory_type.is_shared() {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
        });
    }
    let maximum_pages = memory_type.maximum().ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let initial_pages = u32::try_from(memory_type.minimum()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let declared_maximum_pages = u32::try_from(maximum_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let maximum_pages = maximum_pages_budget
        .map(|budget| declared_maximum_pages.min(budget))
        .unwrap_or(declared_maximum_pages);
    if maximum_pages < initial_pages {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
        });
    }
    Ok(Some(SharedMemorySpec {
        initial_pages,
        maximum_pages,
    }))
}

pub(super) fn define_imported_shared_memory<T>(
    linker: &mut CoreLinker<T>,
    store: &wasmtime::Store<T>,
    module: &Module,
    memory: SharedMemory,
) -> Result<(), ProgramExecError> {
    for import in module.imports() {
        if import.ty().memory().is_some() {
            linker
                .define(store, import.module(), import.name(), memory.clone())
                .map_err(map_program_runtime_error)?;
        }
    }
    Ok(())
}

pub(super) fn fill_random(
    memory: &SharedMemory,
    entropy: &Mutex<crate::EntropyPool>,
    ptr: u32,
    len: u32,
) -> i32 {
    let mut bytes = alloc::vec![0_u8; len as usize];
    if entropy.lock().fill_secure(&mut bytes).is_err() {
        return p1::errno::IO;
    }
    write_shared_memory(memory, ptr, &bytes).map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

pub(super) fn try_read_u32(memory: &SharedMemory, ptr: u32) -> Result<u32, ProgramExecError> {
    let bytes = read_shared_memory(memory, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("u32 read must return 4 bytes")
    })))
}

pub(super) fn write_u32(memory: &SharedMemory, ptr: u32, value: u32) -> i32 {
    write_shared_memory(memory, ptr, &value.to_le_bytes())
        .map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

pub(super) fn write_u64(memory: &SharedMemory, ptr: u32, value: u64) -> i32 {
    write_shared_memory(memory, ptr, &value.to_le_bytes())
        .map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

pub(super) fn read_shared_memory(
    memory: &SharedMemory,
    ptr: u32,
    len: u32,
) -> Result<Vec<u8>, ProgramExecError> {
    let data = memory.data();
    let start = ptr as usize;
    let len = len as usize;
    let end = start.checked_add(len).ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
    })?;
    if end > data.len() {
        tracing::error!(
            start,
            end,
            memory_size = data.len(),
            "compiler plugin memory read is out of bounds"
        );
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    let mut bytes = Vec::with_capacity(len);
    unsafe {
        bytes.extend_from_slice(core::slice::from_raw_parts(
            data.as_ptr().cast::<u8>().add(start),
            len,
        ));
    }
    Ok(bytes)
}

pub(super) fn write_shared_memory(
    memory: &SharedMemory,
    ptr: u32,
    bytes: &[u8],
) -> Result<(), ProgramExecError> {
    let data = memory.data();
    let start = ptr as usize;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
        })?;
    if end > data.len() {
        tracing::error!(
            start,
            end,
            memory_size = data.len(),
            "compiler plugin memory write is out of bounds"
        );
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            data.as_ptr().cast::<u8>().add(start).cast_mut(),
            bytes.len(),
        );
    }
    Ok(())
}
