use super::*;

pub(super) struct Preview1IovLayout {
    pub(super) iovs: Preview1Iovs,
    /// The sum of the iovec lengths: at most `u32::MAX`, the width of
    /// preview1's `size`, by construction.
    pub(super) byte_len: u32,
}

#[derive(Clone, Copy)]
pub(super) struct Preview1Memory {
    pub(super) base: usize,
    pub(super) len: usize,
}

/// The size of a preview1 `iovec`: a `u32` buffer address and a `u32`
/// length (WASI snapshot preview1 `iovec`/`ciovec` layout).
const P1_IOVEC_BYTES: usize = 8;

/// The most iovecs one vectored call takes: POSIX `IOV_MAX` as Linux
/// (`UIO_MAXIOV`) and wasi-libc's `limits.h` define it. A larger count is
/// `EINVAL`, as `readv`/`writev` answer.
pub(super) const P1_IOV_MAX: usize = 1024;

/// The `N` guest bytes at `ptr`, or `EFAULT` when they are not inside
/// `memory`: a fixed-size read that needs no caller and allocates nothing.
pub(super) fn p1_read_array<const N: usize>(
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<[u8; N], i32> {
    let mut bytes = [0_u8; N];
    preview1_read_memory_into(memory, ptr, &mut bytes).map_err(|_| p1::errno::FAULT)?;
    Ok(bytes)
}

/// How many random bytes one pass of [`p1_fill_random`] draws into its
/// stack buffer: one page of the kernel stack.
pub(super) const P1_RANDOM_CHUNK_BYTES: usize = 4096;

/// Fills the `len` guest bytes at `ptr` from `fill`, a page at a time through
/// a stack buffer. The range is validated against `memory` first, so a
/// length no memory could hold is `EFAULT` and `fill` never runs, and what
/// the call holds does not grow with `len`.
pub(super) fn p1_fill_random(
    memory: Preview1Memory,
    ptr: GuestAddress,
    len: u32,
    mut fill: impl FnMut(&mut [u8]),
) -> i32 {
    let len = len as usize;
    if let Err(errno) = p1_checked_guest_array(memory.len, ptr, len, 1) {
        return errno;
    }
    let mut chunk = [0_u8; P1_RANDOM_CHUNK_BYTES];
    let mut done = 0;
    while done < len {
        let take = (len - done).min(P1_RANDOM_CHUNK_BYTES);
        fill(&mut chunk[..take]);
        let status = preview1_write_memory(memory, ptr.offset(done as u64), &chunk[..take]);
        if status != p1::errno::SUCCESS {
            return status;
        }
        done += take;
    }
    p1::errno::SUCCESS
}

/// The offset of the `count` elements of `element` bytes each that start at
/// `ptr`, or `EFAULT` when they do not lie wholly inside the `memory_len`
/// bytes of the caller's linear memory.
///
/// Every host call that sizes anything from a guest count or length passes
/// it through here first, so a length no memory could hold is refused
/// before a kernel allocation is made for it.
pub(super) fn p1_checked_guest_array(
    memory_len: usize,
    ptr: GuestAddress,
    count: usize,
    element: usize,
) -> Result<usize, i32> {
    let start = usize::try_from(ptr.raw()).map_err(|_| p1::errno::FAULT)?;
    let len = count.checked_mul(element).ok_or(p1::errno::FAULT)?;
    let end = start.checked_add(len).ok_or(p1::errno::FAULT)?;
    if end > memory_len {
        return Err(p1::errno::FAULT);
    }
    Ok(start)
}

pub(super) fn take_preview1_carry(carry: &mut Bytes, max_bytes: usize) -> Bytes {
    if carry.len() <= max_bytes {
        core::mem::take(carry)
    } else {
        carry.split_to(max_bytes)
    }
}

pub(super) fn p1_read_path<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    path: GuestAddress,
    path_len: u32,
) -> Result<String, ProgramExecError> {
    p1_read_memory(caller, memory, path, path_len as usize).and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidPath,
            detail: ProgramExecErrorDetail::InvalidProgramPathEncoding,
        })
    })
}

pub(super) fn nul_terminated_list_size<'a>(
    mut values: impl Iterator<Item = &'a str>,
) -> Option<u32> {
    values.try_fold(0u32, |acc, value| {
        let len = u32::try_from(value.len()).ok()?;
        acc.checked_add(len)?.checked_add(1)
    })
}

pub(super) fn p1_write_string_array<'a, CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    pointers: GuestAddress,
    buffer: GuestAddress,
    values: impl Iterator<Item = &'a str>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let mut current = buffer;
    let mut status = p1::errno::SUCCESS;
    for (index, value) in values.enumerate() {
        let pointer = pointers.offset(index as u64 * 4);
        // The guest pointer table holds 32-bit addresses; an address past the
        // 32-bit guest space is unrepresentable and faults.
        let Ok(string_address) = u32::try_from(current.raw()) else {
            return p1::errno::FAULT;
        };
        status = status.max(p1_write_u32(caller, memory, pointer, string_address));
        status = status.max(p1_write_memory(caller, memory, current, value.as_bytes()));
        let terminator = current.offset(value.len() as u64);
        status = status.max(p1_write_u8(caller, memory, terminator, 0));
        current = terminator.offset(1);
    }
    status
}

pub(super) fn p1_memory<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    if let Some(memory) = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
    {
        let data = memory.data(&mut *caller);
        return Some(Preview1Memory {
            base: data.as_ptr() as usize,
            len: data.len(),
        });
    }
    let shared_memory = caller.data().imported_memory.as_ref()?;
    let data = shared_memory.data();
    Some(Preview1Memory {
        base: data.as_ptr().cast::<u8>() as usize,
        len: data.len(),
    })
}

pub(super) fn p1_memory_from_instance<CpuImpl, Net, StorageImpl>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    instance: &wasmtime::Instance,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    if let Some(memory) = instance.get_memory(&mut *store, "memory") {
        let data = memory.data(&mut *store);
        return Some(Preview1Memory {
            base: data.as_ptr() as usize,
            len: data.len(),
        });
    }
    let shared_memory = store.data().imported_memory.as_ref()?;
    let data = shared_memory.data();
    Some(Preview1Memory {
        base: data.as_ptr().cast::<u8>() as usize,
        len: data.len(),
    })
}

pub(super) fn preview1_read_memory(
    memory: Preview1Memory,
    ptr: GuestAddress,
    len: usize,
) -> Result<Vec<u8>, ProgramExecError> {
    let start = preview1_memory_start(memory, ptr, len)?;
    // SAFETY: the bounds check above proves `start..start + len` lies
    // inside the guest's linear memory, which stays mapped for as long
    // as the borrow of `memory` lasts.
    let source = unsafe { core::slice::from_raw_parts((memory.base as *const u8).add(start), len) };
    Ok(source.to_vec())
}

pub(super) fn preview1_read_memory_into(
    memory: Preview1Memory,
    ptr: GuestAddress,
    bytes: &mut [u8],
) -> Result<(), ProgramExecError> {
    let start = preview1_memory_start(memory, ptr, bytes.len())?;
    // SAFETY: preview1/WASIX host calls run synchronously on the owning
    // store task. The bounds check above proves the source range lies inside
    // the guest memory view captured for this host call.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (memory.base as *const u8).add(start),
            bytes.as_mut_ptr(),
            bytes.len(),
        );
    }
    Ok(())
}

pub(super) fn preview1_memory_start(
    memory: Preview1Memory,
    ptr: GuestAddress,
    len: usize,
) -> Result<usize, ProgramExecError> {
    let overflow = || ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
    };
    let start = usize::try_from(ptr.raw()).map_err(|_| overflow())?;
    let end = start.checked_add(len).ok_or_else(overflow)?;
    if end > memory.len {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    Ok(start)
}

pub(super) fn preview1_write_memory(
    memory: Preview1Memory,
    ptr: GuestAddress,
    bytes: &[u8],
) -> i32 {
    let Ok(start) = usize::try_from(ptr.raw()) else {
        return p1::errno::FAULT;
    };
    let Some(end) = start.checked_add(bytes.len()) else {
        return p1::errno::FAULT;
    };
    if end > memory.len {
        return p1::errno::FAULT;
    }
    // SAFETY: the bounds check above proves the destination range lies inside
    // the guest memory view captured for this host call.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (memory.base as *mut u8).add(start),
            bytes.len(),
        );
    }
    p1::errno::SUCCESS
}

pub(super) fn preview1_read_u32(
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<u32, ProgramExecError> {
    let bytes = preview1_read_memory(memory, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("Preview1 raw u32 read must return exactly 4 bytes")
    })))
}

pub(super) fn preview1_write_u32(memory: Preview1Memory, ptr: GuestAddress, value: u32) -> i32 {
    preview1_write_memory(memory, ptr, &value.to_le_bytes())
}

// Preview1 socket/file profiles charge iov decoding separately from payload
// copy. Keep length accounting in the guest-table decode pass so hot read and
// recv syscalls do not rescan the same SmallVec before touching the real I/O
// path.
pub(super) fn p1_read_iovs_with_byte_len(
    memory: Preview1Memory,
    iovs: GuestAddress,
    iovs_len: u32,
) -> Result<Preview1IovLayout, i32> {
    // An iovec count above `IOV_MAX` is refused before the array is read,
    // and the array and every buffer it names must lie in linear memory
    // before anything is collected for them.
    if iovs_len as usize > P1_IOV_MAX {
        return Err(p1::errno::INVAL);
    }
    p1_checked_guest_array(memory.len, iovs, iovs_len as usize, P1_IOVEC_BYTES)?;
    let read_u32 = |ptr: GuestAddress| p1_read_array(memory, ptr).map(u32::from_le_bytes);
    let mut result = Preview1Iovs::new();
    let mut byte_len = 0_u32;
    for index in 0..iovs_len {
        let iov = iovs.offset(u64::from(index) * P1_IOVEC_BYTES as u64);
        let ptr = read_u32(iov)?;
        let len = read_u32(iov.offset(4))?;
        p1_checked_guest_array(memory.len, GuestAddress::wasm32(ptr), len as usize, 1)?;
        // Iovecs may overlap, so the sum is not bounded by memory; it is
        // bounded by the `size` result type, as POSIX `readv`/`writev`
        // refuse a sum that overflows theirs.
        byte_len = byte_len.checked_add(len).ok_or(p1::errno::INVAL)?;
        result.push((ptr, len));
    }
    Ok(Preview1IovLayout {
        iovs: result,
        byte_len,
    })
}

/// The guest bytes `layout`'s iovecs name, copied out one bounded chunk at
/// a time ([`GUEST_IO_CHUNK_BYTES`]) so a gathered write holds kernel memory
/// for a chunk, never for the whole transfer.
pub(super) struct Preview1Gather {
    memory: Preview1Memory,
    ranges: Preview1IovRanges,
    /// The range the next byte comes from, and how far into it.
    range: usize,
    offset: usize,
    remaining: usize,
}

impl Preview1Gather {
    pub(super) fn new(memory: Preview1Memory, layout: &Preview1IovLayout) -> Result<Self, i32> {
        Ok(Self {
            memory,
            ranges: p1_iovs_memory_ranges(memory, &layout.iovs)?,
            range: 0,
            offset: 0,
            remaining: layout.byte_len as usize,
        })
    }

    /// The bytes not yet taken.
    pub(super) fn remaining(&self) -> usize {
        self.remaining
    }

    /// The length of the next chunk.
    pub(super) fn next_len(&self) -> usize {
        self.remaining.min(GUEST_IO_CHUNK_BYTES)
    }

    /// Copies the next `destination.len()` bytes, which is at most
    /// [`Self::remaining`], into `destination`.
    pub(super) fn fill(&mut self, destination: &mut [u8]) {
        debug_assert!(
            destination.len() <= self.remaining,
            "a gathered chunk is no longer than what is left"
        );
        let taken = destination.len();
        let mut rest = destination;
        while !rest.is_empty() {
            let (start, len) = self.ranges[self.range];
            let available = len - self.offset;
            if available == 0 {
                self.range += 1;
                self.offset = 0;
                continue;
            }
            let take = available.min(rest.len());
            // SAFETY: each range was checked against the guest memory's
            // length (`p1_iovs_memory_ranges`), and guest memory neither
            // moves nor shrinks during the host call that holds `memory`.
            let source = unsafe {
                core::slice::from_raw_parts(
                    (self.memory.base as *const u8).add(start + self.offset),
                    take,
                )
            };
            let (head, tail) = rest.split_at_mut(take);
            head.copy_from_slice(source);
            rest = tail;
            self.offset += take;
        }
        self.remaining -= taken;
    }

    /// The next chunk, or `None` once every byte was taken.
    pub(super) fn next_chunk(&mut self) -> Option<Vec<u8>> {
        if self.remaining == 0 {
            return None;
        }
        let mut chunk = vec![0; self.next_len()];
        self.fill(&mut chunk);
        Some(chunk)
    }

    /// Everything still held, in one piece, for a datagram that can carry
    /// at most `max` bytes. A datagram goes out whole, so a payload the
    /// protocol cannot carry is `EMSGSIZE` before any kernel memory is
    /// held for it.
    pub(super) fn datagram(&mut self, max: usize) -> Result<Vec<u8>, i32> {
        if self.remaining > max {
            return Err(p1::errno::MSGSIZE);
        }
        let mut payload = vec![0; self.remaining];
        self.fill(&mut payload);
        Ok(payload)
    }
}

/// Sends everything `gather` still holds through `send`, one chunk at a
/// time, and reports how many bytes went. An empty transfer reaches `send`
/// once. A refusal after some chunks went leaves them sent: the guest meets
/// the refusal on its next call.
pub(super) async fn p1_send_gathered<SendChunk, ChunkSent>(
    gather: &mut Preview1Gather,
    mut send: SendChunk,
) -> Result<u32, i32>
where
    SendChunk: FnMut(Vec<u8>) -> ChunkSent,
    ChunkSent: Future<Output = Result<(), i32>>,
{
    let mut sent = 0_u32;
    loop {
        let chunk = gather.next_chunk().unwrap_or_default();
        let len = chunk.len();
        match send(chunk).await {
            // The iovecs' sum fits a `u32`, so no count of it overflows.
            Ok(()) => sent += len as u32,
            Err(_) if sent > 0 => break,
            Err(errno) => return Err(errno),
        }
        if gather.remaining() == 0 {
            break;
        }
    }
    Ok(sent)
}

/// A file a gathered write lands in, one chunk at a time.
pub(super) trait Preview1ChunkedFile {
    /// Why the file refused a chunk.
    type Refusal;

    /// Takes the next `len` bytes out of `gather`, lands them at
    /// `position`, and answers the end of the file they reached.
    fn land(
        &mut self,
        gather: &mut Preview1Gather,
        position: WritePosition,
        len: usize,
    ) -> impl Future<Output = Result<u64, Self::Refusal>> + Send;
}

/// Lands everything `gather` still holds in `file`, one chunk at a time
/// from `position`, and reports the end of the file the last chunk reached
/// and how many bytes landed. An empty transfer reaches the file once, as
/// it reports the end of the file an append reached.
///
/// Every chunk of an append goes to the end the file has when it arrives,
/// so it never lands on what another writer appended in between; a
/// positioned write continues where its last chunk ended. A refusal after
/// some chunks landed is a short write: the guest meets the refusal on its
/// next call.
pub(super) async fn p1_write_gathered<File: Preview1ChunkedFile + Send>(
    gather: &mut Preview1Gather,
    position: WritePosition,
    file: &mut File,
) -> Result<(u64, u32), File::Refusal> {
    let len = gather.next_len();
    let mut end = file.land(gather, position, len).await?;
    // The iovecs' sum fits a `u32`, so no count of it overflows.
    let mut written = len as u32;
    while gather.remaining() > 0 {
        let next = match position {
            WritePosition::End => WritePosition::End,
            WritePosition::At(_) => WritePosition::At(end),
        };
        let len = gather.next_len();
        let Ok(chunk_end) = file.land(gather, next, len).await else {
            break;
        };
        end = chunk_end;
        written += len as u32;
    }
    Ok((end, written))
}

pub(super) fn p1_iovs_memory_ranges(
    memory: Preview1Memory,
    iovs: &[(u32, u32)],
) -> Result<Preview1IovRanges, i32> {
    let mut ranges = Preview1IovRanges::with_capacity(iovs.len());
    for (ptr, len) in iovs {
        let len = usize::try_from(*len).map_err(|_| p1::errno::OVERFLOW)?;
        let start = p1_checked_guest_array(memory.len, GuestAddress::wasm32(*ptr), len, 1)?;
        ranges.push((start, len));
    }
    Ok(ranges)
}

pub(super) fn p1_read_memory<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    len: usize,
) -> Result<Vec<u8>, ProgramExecError> {
    preview1_read_memory(memory, ptr, len)
}

pub(super) fn p1_read_memory_into<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    bytes: &mut [u8],
) -> Result<(), ProgramExecError> {
    preview1_read_memory_into(memory, ptr, bytes)
}

pub(super) fn p1_write_memory<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    bytes: &[u8],
) -> i32 {
    preview1_write_memory(memory, ptr, bytes)
}

pub(super) fn p1_try_read_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<u32, ProgramExecError> {
    let mut bytes = [0_u8; 4];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(super) fn p1_try_read_u8<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<u8, ProgramExecError> {
    let mut bytes = [0_u8; 1];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(bytes[0])
}

pub(super) fn p1_try_read_u16<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<u16, ProgramExecError> {
    let mut bytes = [0_u8; 2];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

pub(super) fn p1_try_read_u64<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<u64, ProgramExecError> {
    let mut bytes = [0_u8; 8];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub(super) fn p1_write_u8<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    value: u8,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &[value])
}

pub(super) fn p1_write_u16<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    value: u16,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

pub(super) fn p1_write_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    value: u32,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

pub(super) fn p1_write_u64<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    value: u64,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

pub(super) fn p1_write_iovs_from_bytes<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: Preview1Iovs,
    bytes: &[u8],
    written_out: GuestAddress,
) -> i32 {
    let mut copied = 0usize;
    for (ptr, len) in iovs {
        if copied >= bytes.len() {
            break;
        }
        let len = (len as usize).min(bytes.len() - copied);
        let status = p1_write_memory(
            caller,
            memory,
            GuestAddress::wasm32(ptr),
            &bytes[copied..copied + len],
        );
        if status != p1::errno::SUCCESS {
            return status;
        }
        copied += len;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, written_out, copied)
}
