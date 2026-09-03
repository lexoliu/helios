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
    /// Recycles claimed by `reserve_for_recycle` whose scrub has not yet
    /// landed in a bucket. Spawns that lose the allocation race wait on
    /// `recycled` instead of failing while this is non-zero.
    pending_scrubs: usize,
    recycled: Arc<crate::exec::Notify>,
}

impl SharedMemoryPool {
    pub(super) fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            buckets: HashMap::new(),
            pending_scrubs: 0,
            recycled: Arc::new(crate::exec::Notify::new()),
        }
    }

    /// Hands out a pre-zeroed pooled memory, or a fresh one. Pooled
    /// entries were scrubbed off the spawn path by the recycle task, so
    /// this never zeroes memory on the process-start critical path.
    /// The pool is a cache: when a fresh allocation fails, entries
    /// retained under other specs are evicted (freeing their user RAM)
    /// and the allocation retried before the failure propagates.
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

        loop {
            match SharedMemory::new(engine, spec.memory_type()) {
                Ok(memory) => return Ok(memory),
                Err(error) => {
                    if !self.evict_one() {
                        return Err(map_program_runtime_error(error));
                    }
                }
            }
        }
    }

    /// Drops one pooled memory, releasing its user RAM and budget.
    /// Returns false when every bucket is empty.
    pub(super) fn evict_one(&mut self) -> bool {
        let Some(spec) = self
            .buckets
            .iter()
            .find_map(|(spec, bucket)| (!bucket.is_empty()).then_some(*spec))
        else {
            return false;
        };
        let memory = self
            .buckets
            .get_mut(&spec)
            .and_then(|bucket| bucket.pop())
            .expect("shared-memory pool eviction found an empty bucket");
        self.resident_bytes = self
            .resident_bytes
            .checked_sub(spec.byte_size())
            .expect("shared-memory pool byte accounting underflow");
        drop(memory);
        true
    }

    /// Claims budget for a memory about to be scrubbed and re-pooled.
    /// The bytes count as resident from this point so a burst of exits
    /// cannot overshoot the pool budget while scrubs are in flight.
    pub(super) fn reserve_for_recycle(
        &mut self,
        spec: SharedMemorySpec,
        memory: &SharedMemory,
    ) -> bool {
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
        self.pending_scrubs += 1;
        true
    }

    /// Returns a scrubbed memory to its bucket. Budget was claimed by
    /// `reserve_for_recycle`.
    pub(super) fn finish_recycle(&mut self, spec: SharedMemorySpec, memory: SharedMemory) {
        self.buckets.entry(spec).or_default().push(memory);
        self.pending_scrubs = self
            .pending_scrubs
            .checked_sub(1)
            .expect("shared-memory pool finished a recycle it never reserved");
        self.recycled.notify_one();
    }
}

/// Acquires a shared memory, waiting for in-flight recycles when a fresh
/// allocation fails. Fast spawn/exit cycles can outrun the background
/// scrub: the exited guest's memory holds pool budget (and user RAM)
/// until its scrub lands, so a burst of spawns would otherwise allocate
/// fresh multi-hundred-megabyte memories until the user pool is empty.
/// Waiting turns that transient shortage into backpressure; the error
/// only propagates once no recycle is in flight to satisfy the retry.
pub(super) async fn acquire_or_wait_for_recycle(
    pool: &Mutex<SharedMemoryPool>,
    engine: &wasmtime::Engine,
    spec: SharedMemorySpec,
) -> Result<SharedMemory, ProgramExecError> {
    loop {
        let recycled = {
            let mut guard = pool.lock();
            match guard.acquire(engine, spec) {
                Ok(memory) => return Ok(memory),
                Err(error) => {
                    if guard.pending_scrubs == 0 {
                        return Err(error);
                    }
                    guard.recycled.clone()
                }
            }
        };
        // The kernel Notify stores permits, so a recycle that lands
        // between releasing the lock and awaiting is still observed.
        recycled.notified().await;
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

/// Bytes zeroed between cooperative yields while scrubbing. Sized so a
/// 512 MiB memory turns around in ~128 yields: spawn bursts wait on the
/// scrub through `acquire_or_wait_for_recycle`, so recycle latency is
/// spawn latency under pressure, while each chunk still clears in well
/// under a millisecond and keeps the executor responsive.
const SCRUB_CHUNK_BYTES: usize = 4 * 1024 * 1024;

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

/// The largest shared-memory maximum the user pool can still commit.
///
/// Backends without lazy-commit virtual memory commit a shared memory's
/// whole maximum up front, so every maximum this kernel hands out has to
/// be one the user pool can actually satisfy *now*. Two properties make
/// the naive "whatever is free" answer wrong, and both are covered here:
/// the pool rounds every request up to a power of two, so a request for
/// the raw free byte count is refused by construction, and the maximum
/// is capped by [`PROGRAM_SHARED_MEMORY_MAX_PAGES`] so one program
/// cannot claim a pool that later programs still need.
pub(super) fn user_shared_memory_budget_pages() -> u32 {
    let allocatable_pages = user_heap_stats().largest_allocatable_bytes() / WASM_PAGE_SIZE;
    let allocatable_pages = u32::try_from(allocatable_pages).unwrap_or(u32::MAX);
    allocatable_pages.min(PROGRAM_SHARED_MEMORY_MAX_PAGES)
}

pub(super) fn imported_shared_memory_spec_with_user_budget(
    module: &Module,
) -> Result<Option<SharedMemorySpec>, ProgramExecError> {
    imported_shared_memory_spec(module, Some(user_shared_memory_budget_pages()))
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
    let maximum_pages = memory_type.maximum().ok_or(ProgramExecError {
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
    entropy.lock().fill_secure(&mut bytes);
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
    let end = start.checked_add(len).ok_or(ProgramExecError {
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
    let end = start.checked_add(bytes.len()).ok_or(ProgramExecError {
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
