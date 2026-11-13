use helios_hal::vmm::{VirtAddr, VirtRange};
use thiserror::Error;

use super::*;
use crate::memory::{GuestResidency, SwapFaultError};

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

/// A guest's linear memory as an async host function reaches it.
///
/// An async host function is polled inside its fiber's `block_on`, which
/// holds the fiber's blocking context, so a swapped-out page it touched
/// could not be faulted in where it stands. This type therefore offers no
/// view of the memory that outlives a check: the only way to its bytes
/// is [`Self::read`] or [`Self::write`], each of which finds every page
/// of the range present and runs its synchronous closure in that same
/// poll (`GuestResidency::with_present`). A copy after an await is a new
/// call, with a check of its own.
///
/// What a synchronous host call holds is [`Preview1Memory`]: its fiber's
/// blocking context is free, so a fault there is served in place.
pub(super) struct Preview1AsyncMemory {
    memory: Preview1Memory,
    /// `None` on a machine that cannot swap, where every page is present.
    residency: Option<GuestResidency>,
}

/// Why an async host function could not reach guest bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(super) enum GuestAccessError {
    /// The range is not inside the guest's linear memory: the guest's
    /// mistake, answered with `EFAULT`.
    #[error("the range is not inside the guest's linear memory")]
    OutOfBounds,
    /// A page of the range is gone and could not be read back: the
    /// guest's memory is lost, and the guest traps.
    #[error("a page of the range could not be swapped back in: {0}")]
    Swap(#[from] SwapFaultError),
}

/// What ends an async host call before it reaches its own result: the
/// errno it answers the guest with, or the loss of a guest page that traps
/// the guest.
///
/// A guest address out of bounds is the errno `EFAULT`, which is what
/// converting a [`GuestAccessError`] gives, so a body reaches guest memory
/// with `?` and answers every other failure with the errno it computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum P1Stop {
    Errno(i32),
    Trap(SwapFaultError),
}

impl From<i32> for P1Stop {
    fn from(errno: i32) -> Self {
        Self::Errno(errno)
    }
}

impl From<GuestAccessError> for P1Stop {
    fn from(error: GuestAccessError) -> Self {
        match error {
            GuestAccessError::OutOfBounds => Self::Errno(p1::errno::FAULT),
            GuestAccessError::Swap(error) => Self::Trap(error),
        }
    }
}

/// The result an async host function hands the runtime for its body's
/// answer: the errno, or a trap for the guest whose page was lost.
pub(super) fn p1_reply(answer: Result<i32, P1Stop>) -> wasmtime::Result<i32> {
    match answer {
        Ok(errno) | Err(P1Stop::Errno(errno)) => Ok(errno),
        Err(P1Stop::Trap(error)) => Err(wasmtime::Error::new(error)),
    }
}

impl Preview1AsyncMemory {
    pub(super) fn new(memory: Preview1Memory) -> Self {
        Self {
            memory,
            residency: GuestResidency::installed(),
        }
    }

    /// The length of the guest's linear memory.
    pub(super) fn len(&self) -> usize {
        self.memory.len
    }

    /// The `N` guest bytes at `ptr`.
    pub(super) async fn read_array<const N: usize>(
        &self,
        ptr: GuestAddress,
    ) -> Result<[u8; N], GuestAccessError> {
        self.read(ptr, N, |bytes| {
            let mut array = [0_u8; N];
            array.copy_from_slice(bytes);
            array
        })
        .await
    }

    pub(super) async fn read_u8(&self, ptr: GuestAddress) -> Result<u8, GuestAccessError> {
        Ok(self.read_array::<1>(ptr).await?[0])
    }

    pub(super) async fn read_u16(&self, ptr: GuestAddress) -> Result<u16, GuestAccessError> {
        self.read_array(ptr).await.map(u16::from_le_bytes)
    }

    pub(super) async fn read_u32(&self, ptr: GuestAddress) -> Result<u32, GuestAccessError> {
        self.read_array(ptr).await.map(u32::from_le_bytes)
    }

    pub(super) async fn read_u64(&self, ptr: GuestAddress) -> Result<u64, GuestAccessError> {
        self.read_array(ptr).await.map(u64::from_le_bytes)
    }

    /// An owned copy of the `len` guest bytes at `ptr`. The range is
    /// checked against linear memory before anything is allocated for it.
    pub(super) async fn read_bytes(
        &self,
        ptr: GuestAddress,
        len: usize,
    ) -> Result<Vec<u8>, GuestAccessError> {
        self.read(ptr, len, <[u8]>::to_vec).await
    }

    /// The UTF-8 string of `len` bytes at `ptr`. A range outside linear
    /// memory and bytes that are not UTF-8 are both `EFAULT`.
    pub(super) async fn read_path(&self, ptr: GuestAddress, len: u32) -> Result<String, P1Stop> {
        let bytes = self.read_bytes(ptr, len as usize).await?;
        String::from_utf8(bytes).map_err(|_| P1Stop::Errno(p1::errno::FAULT))
    }

    /// Copies `bytes` to the guest at `ptr`.
    pub(super) async fn write_bytes(
        &self,
        ptr: GuestAddress,
        bytes: &[u8],
    ) -> Result<(), GuestAccessError> {
        self.write(ptr, bytes.len(), |destination| {
            destination.copy_from_slice(bytes)
        })
        .await
    }

    pub(super) async fn write_u8(
        &self,
        ptr: GuestAddress,
        value: u8,
    ) -> Result<(), GuestAccessError> {
        self.write_bytes(ptr, &[value]).await
    }

    pub(super) async fn write_u16(
        &self,
        ptr: GuestAddress,
        value: u16,
    ) -> Result<(), GuestAccessError> {
        self.write_bytes(ptr, &value.to_le_bytes()).await
    }

    pub(super) async fn write_u32(
        &self,
        ptr: GuestAddress,
        value: u32,
    ) -> Result<(), GuestAccessError> {
        self.write_bytes(ptr, &value.to_le_bytes()).await
    }

    pub(super) async fn write_u64(
        &self,
        ptr: GuestAddress,
        value: u64,
    ) -> Result<(), GuestAccessError> {
        self.write_bytes(ptr, &value.to_le_bytes()).await
    }

    /// Scatters `bytes` over the buffers `iovs` name, in order, each as
    /// far as `bytes` reaches, and writes how many went to `written_out`.
    /// A buffer is brought in before it is written, so a swapped-out one
    /// waits for its page; the buffers already written stand if a later
    /// one is out of bounds.
    pub(super) async fn write_iovs_from_bytes(
        &self,
        iovs: &[(u32, u32)],
        bytes: &[u8],
        written_out: GuestAddress,
    ) -> Result<(), P1Stop> {
        let mut copied = 0usize;
        for &(ptr, len) in iovs {
            if copied >= bytes.len() {
                break;
            }
            let len = (len as usize).min(bytes.len() - copied);
            self.write_bytes(GuestAddress::wasm32(ptr), &bytes[copied..copied + len])
                .await?;
            copied += len;
        }
        let copied = u32::try_from(copied).map_err(|_| P1Stop::Errno(p1::errno::OVERFLOW))?;
        Ok(self.write_u32(written_out, copied).await?)
    }

    /// Runs `access` over the `len` guest bytes at `ptr` once they are
    /// all present, in the poll that found them so.
    pub(super) async fn read<R>(
        &self,
        ptr: GuestAddress,
        len: usize,
        access: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, GuestAccessError> {
        let start = self.checked_host_address(ptr, len)?;
        self.present(start, len, || {
            // SAFETY: `checked_host_address` proved the range lies inside
            // the guest's linear memory, which never moves or shrinks while
            // the instance lives, and `present` runs this in the poll that
            // found every page of it present.
            access(unsafe { core::slice::from_raw_parts(start as *const u8, len) })
        })
        .await
    }

    /// Runs `access` over the `len` guest bytes at `ptr`, mutably, once
    /// they are all present, in the poll that found them so.
    pub(super) async fn write<R>(
        &self,
        ptr: GuestAddress,
        len: usize,
        access: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, GuestAccessError> {
        let start = self.checked_host_address(ptr, len)?;
        self.present(start, len, || {
            // SAFETY: as in `read`; the guest is suspended in this host
            // call, so nothing else on its store touches the range while
            // the closure runs.
            access(unsafe { core::slice::from_raw_parts_mut(start as *mut u8, len) })
        })
        .await
    }

    /// The host address of the `len` guest bytes at `ptr`, or
    /// [`GuestAccessError::OutOfBounds`].
    fn checked_host_address(
        &self,
        ptr: GuestAddress,
        len: usize,
    ) -> Result<usize, GuestAccessError> {
        let start = p1_checked_guest_array(self.memory.len, ptr, len, 1)
            .map_err(|_| GuestAccessError::OutOfBounds)?;
        Ok(self.memory.base + start)
    }

    async fn present<R>(
        &self,
        start: usize,
        len: usize,
        access: impl FnOnce() -> R,
    ) -> Result<R, GuestAccessError> {
        match &self.residency {
            Some(residency) => Ok(residency
                .with_present(VirtRange::new(VirtAddr::new(start), len), access)
                .await?),
            None => Ok(access()),
        }
    }
}

/// The caller's linear memory, as an async host function reaches it.
pub(super) fn p1_async_memory<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Option<Preview1AsyncMemory>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    p1_memory(caller).map(Preview1AsyncMemory::new)
}

/// The most guest bytes one host call holds in kernel memory at once when
/// it has to copy them out of linear memory. A transfer larger than this
/// moves in successive chunks, so what a call holds never grows with the
/// guest's length. 64 KiB is the largest UDP payload (`u16::MAX` rounded to
/// a power of two) and the WASI UDP default buffer size
/// (`DEFAULT_WASI_UDP_BUFFER_BYTES`).
pub(super) const P1_IO_CHUNK_BYTES: usize = 64 * 1024;

/// The size of a preview1 `iovec`: a `u32` buffer address and a `u32`
/// length (WASI snapshot preview1 `iovec`/`ciovec` layout).
const P1_IOVEC_BYTES: usize = 8;

/// The most iovecs one vectored call takes: POSIX `IOV_MAX` as Linux
/// (`UIO_MAXIOV`) and wasi-libc's `limits.h` define it. A larger count is
/// `EINVAL`, as `readv`/`writev` answer.
pub(super) const P1_IOV_MAX: usize = 1024;

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

pub(super) fn p1_write_string_array<'a, CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    pointers: GuestAddress,
    buffer: GuestAddress,
    values: impl Iterator<Item = &'a str>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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

pub(super) fn p1_memory<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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

pub(super) fn p1_memory_from_instance<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    instance: &wasmtime::Instance,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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
pub(super) async fn p1_read_iovs_with_byte_len(
    memory: &Preview1AsyncMemory,
    iovs: GuestAddress,
    iovs_len: u32,
) -> Result<Preview1IovLayout, P1Stop> {
    // An iovec count above `IOV_MAX` is refused before the array is read,
    // and the array and every buffer it names must lie in linear memory
    // before anything is collected for them.
    if iovs_len as usize > P1_IOV_MAX {
        return Err(P1Stop::Errno(p1::errno::INVAL));
    }
    let memory_len = memory.len();
    let layout = memory
        .read(iovs, iovs_len as usize * P1_IOVEC_BYTES, |table| {
            let mut result = Preview1Iovs::new();
            let mut byte_len = 0_u32;
            for entry in table.as_chunks::<P1_IOVEC_BYTES>().0 {
                let (ptr, len) = entry.split_at(4);
                let ptr = u32::from_le_bytes(ptr.try_into().expect("an iovec holds a u32 address"));
                let len = u32::from_le_bytes(len.try_into().expect("an iovec holds a u32 length"));
                p1_checked_guest_array(memory_len, GuestAddress::wasm32(ptr), len as usize, 1)?;
                // Iovecs may overlap, so the sum is not bounded by memory; it is
                // bounded by the `size` result type, as POSIX `readv`/`writev`
                // refuse a sum that overflows theirs.
                byte_len = byte_len.checked_add(len).ok_or(p1::errno::INVAL)?;
                result.push((ptr, len));
            }
            Ok::<_, i32>(Preview1IovLayout {
                iovs: result,
                byte_len,
            })
        })
        .await?;
    Ok(layout?)
}

/// The guest bytes `layout`'s iovecs name, copied out one bounded chunk at
/// a time ([`P1_IO_CHUNK_BYTES`]) so a gathered write holds kernel memory
/// for a chunk, never for the whole transfer.
///
/// Each span of a chunk is read through the accessor, so a span whose page
/// is swapped out waits for it, and nothing but the position of the next
/// byte is kept across that wait: a chunk that stops part-way leaves the
/// gather at the last span it finished.
pub(super) struct Preview1Gather {
    iovs: Preview1Iovs,
    /// The iovec the next byte comes from, and how far into it.
    range: usize,
    offset: usize,
    remaining: usize,
}

impl Preview1Gather {
    pub(super) fn new(
        memory: &Preview1AsyncMemory,
        layout: &Preview1IovLayout,
    ) -> Result<Self, i32> {
        for &(ptr, len) in &layout.iovs {
            p1_checked_guest_array(memory.len(), GuestAddress::wasm32(ptr), len as usize, 1)?;
        }
        Ok(Self {
            iovs: layout.iovs.clone(),
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
        self.remaining.min(P1_IO_CHUNK_BYTES)
    }

    /// Copies the next `destination.len()` bytes, which is at most
    /// [`Self::remaining`], into `destination`.
    pub(super) async fn fill(
        &mut self,
        memory: &Preview1AsyncMemory,
        destination: &mut [u8],
    ) -> Result<(), GuestAccessError> {
        debug_assert!(
            destination.len() <= self.remaining,
            "a gathered chunk is no longer than what is left"
        );
        let mut rest = destination;
        while !rest.is_empty() {
            let (ptr, len) = self.iovs[self.range];
            let available = len as usize - self.offset;
            if available == 0 {
                self.range += 1;
                self.offset = 0;
                continue;
            }
            let take = available.min(rest.len());
            let (head, tail) = rest.split_at_mut(take);
            let source = GuestAddress::wasm32(ptr).offset(self.offset as u64);
            memory
                .read(source, take, |bytes| head.copy_from_slice(bytes))
                .await?;
            rest = tail;
            self.offset += take;
            self.remaining -= take;
        }
        Ok(())
    }

    /// The next chunk, or `None` once every byte was taken.
    pub(super) async fn next_chunk(
        &mut self,
        memory: &Preview1AsyncMemory,
    ) -> Result<Option<Vec<u8>>, GuestAccessError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut chunk = vec![0; self.next_len()];
        self.fill(memory, &mut chunk).await?;
        Ok(Some(chunk))
    }

    /// Everything still held, in one piece, for a datagram that can carry
    /// at most `max` bytes. A datagram goes out whole, so a payload the
    /// protocol cannot carry is `EMSGSIZE` before any kernel memory is
    /// held for it.
    pub(super) async fn datagram(
        &mut self,
        memory: &Preview1AsyncMemory,
        max: usize,
    ) -> Result<Vec<u8>, P1Stop> {
        if self.remaining > max {
            return Err(P1Stop::Errno(p1::errno::MSGSIZE));
        }
        let mut payload = vec![0; self.remaining];
        self.fill(memory, &mut payload).await?;
        Ok(payload)
    }
}

/// Sends everything `gather` still holds through `send`, one chunk at a
/// time, and reports how many bytes went. An empty transfer reaches `send`
/// once. A refusal after some chunks went leaves them sent: the guest meets
/// the refusal on its next call.
pub(super) async fn p1_send_gathered<SendChunk, ChunkSent>(
    memory: &Preview1AsyncMemory,
    gather: &mut Preview1Gather,
    mut send: SendChunk,
) -> Result<u32, P1Stop>
where
    SendChunk: FnMut(Vec<u8>) -> ChunkSent,
    ChunkSent: Future<Output = Result<(), i32>>,
{
    let mut sent = 0_u32;
    loop {
        let chunk = gather.next_chunk(memory).await?.unwrap_or_default();
        let len = chunk.len();
        match send(chunk).await {
            // The iovecs' sum fits a `u32`, so no count of it overflows.
            Ok(()) => sent += len as u32,
            Err(_) if sent > 0 => break,
            Err(errno) => return Err(P1Stop::Errno(errno)),
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

    /// Lands `chunk` at `position`, and answers the end of the file it
    /// reached.
    fn land(
        &mut self,
        chunk: &[u8],
        position: WritePosition,
    ) -> impl Future<Output = Result<u64, Self::Refusal>> + Send;
}

/// What a gathered write to a file came to: the end of the file its last
/// chunk reached and how many bytes landed, or why the file refused the
/// first chunk.
pub(super) type Preview1Landed<Refusal> = Result<(u64, u32), Refusal>;

/// Lands everything `gather` still holds in `file`, one chunk at a time
/// from `position`, and reports the end of the file the last chunk reached
/// and how many bytes landed. An empty transfer reaches the file once, as
/// it reports the end of the file an append reached.
///
/// Every chunk of an append goes to the end the file has when it arrives,
/// so it never lands on what another writer appended in between; a
/// positioned write continues where its last chunk ended. A refusal after
/// some chunks landed is a short write: the guest meets the refusal on its
/// next call. A chunk is read out of guest memory before it is landed, so a
/// page that has to come back from swap holds up the write and never the
/// file.
pub(super) async fn p1_write_gathered<File: Preview1ChunkedFile + Send>(
    memory: &Preview1AsyncMemory,
    gather: &mut Preview1Gather,
    position: WritePosition,
    file: &mut File,
) -> Result<Preview1Landed<File::Refusal>, GuestAccessError> {
    let chunk = gather.next_chunk(memory).await?.unwrap_or_default();
    let mut end = match file.land(&chunk, position).await {
        Ok(end) => end,
        Err(refusal) => return Ok(Err(refusal)),
    };
    // The iovecs' sum fits a `u32`, so no count of it overflows.
    let mut written = chunk.len() as u32;
    while let Some(chunk) = gather.next_chunk(memory).await? {
        let next = match position {
            WritePosition::End => WritePosition::End,
            WritePosition::At(_) => WritePosition::At(end),
        };
        let Ok(chunk_end) = file.land(&chunk, next).await else {
            break;
        };
        end = chunk_end;
        written += chunk.len() as u32;
    }
    Ok(Ok((end, written)))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest memory of `len` bytes, backed by `backing`, on a machine
    /// that cannot swap. What the accessor does once pages go missing is
    /// `GuestResidency::with_present`'s, tested with the swap service.
    fn memory_over(backing: &mut [u8]) -> Preview1AsyncMemory {
        Preview1AsyncMemory {
            memory: Preview1Memory {
                base: backing.as_mut_ptr() as usize,
                len: backing.len(),
            },
            residency: None,
        }
    }

    fn run<F: Future>(future: F) -> F::Output {
        futures_lite::future::block_on(future)
    }

    #[test]
    fn a_write_lands_on_exactly_the_bytes_it_names() {
        let mut backing = vec![0_u8; 64];
        let memory = memory_over(&mut backing);
        run(memory.write(GuestAddress::wasm32(8), 4, |bytes| {
            bytes.copy_from_slice(&0xdead_beef_u32.to_le_bytes())
        }))
        .expect("in bounds");
        let mut expected = vec![0_u8; 64];
        expected[8..12].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
        assert_eq!(backing, expected);
    }

    #[test]
    fn a_read_sees_the_bytes_it_names() {
        let mut backing: Vec<u8> = (0..64).collect();
        let memory = memory_over(&mut backing);
        let seen =
            run(memory.read(GuestAddress::wasm32(60), 4, <[u8]>::to_vec)).expect("in bounds");
        assert_eq!(seen, [60, 61, 62, 63]);
    }

    /// A range that runs past the end of linear memory is the guest's
    /// mistake, and the closure never sees any of it.
    #[test]
    fn a_range_past_the_end_is_refused_before_anything_is_touched() {
        let mut backing = vec![0_u8; 64];
        let memory = memory_over(&mut backing);
        let mut ran = false;
        assert_eq!(
            run(memory.write(GuestAddress::wasm32(61), 4, |_| ran = true)),
            Err(GuestAccessError::OutOfBounds)
        );
        assert_eq!(
            run(memory.read(GuestAddress::wasm32(u32::MAX), 2, |_| ())),
            Err(GuestAccessError::OutOfBounds)
        );
        assert!(!ran, "an out-of-bounds access never reaches its closure");
        assert_eq!(backing, vec![0_u8; 64]);
    }

    /// Every width reads little-endian, as the guest stores it, and a
    /// write of the same width reads back as what was written.
    #[test]
    fn integers_round_trip_little_endian_at_every_width() {
        let mut backing = vec![0_u8; 64];
        let memory = memory_over(&mut backing);
        run(async {
            memory.write_u8(GuestAddress::wasm32(1), 0xa5).await?;
            memory.write_u16(GuestAddress::wasm32(2), 0x1234).await?;
            memory
                .write_u32(GuestAddress::wasm32(8), 0xdead_beef)
                .await?;
            memory
                .write_u64(GuestAddress::wasm32(16), 0x0102_0304_0506_0708)
                .await?;
            assert_eq!(memory.read_u8(GuestAddress::wasm32(1)).await?, 0xa5);
            assert_eq!(memory.read_u16(GuestAddress::wasm32(2)).await?, 0x1234);
            assert_eq!(memory.read_u32(GuestAddress::wasm32(8)).await?, 0xdead_beef);
            assert_eq!(
                memory.read_u64(GuestAddress::wasm32(16)).await?,
                0x0102_0304_0506_0708
            );
            assert_eq!(
                memory.read_array::<2>(GuestAddress::wasm32(2)).await?,
                [0x34, 0x12]
            );
            Ok::<_, GuestAccessError>(())
        })
        .expect("in bounds");
        assert_eq!(backing[2..4], [0x34, 0x12]);
        assert_eq!(backing[8..12], [0xef, 0xbe, 0xad, 0xde]);
    }

    /// A fixed-width access that runs past the end is refused whole.
    #[test]
    fn a_wide_access_straddling_the_end_is_refused() {
        let mut backing = vec![0_u8; 8];
        let memory = memory_over(&mut backing);
        assert_eq!(
            run(memory.write_u64(GuestAddress::wasm32(4), u64::MAX)),
            Err(GuestAccessError::OutOfBounds)
        );
        assert_eq!(
            run(memory.read_u32(GuestAddress::wasm32(6))),
            Err(GuestAccessError::OutOfBounds)
        );
        assert_eq!(backing, vec![0_u8; 8], "nothing of a refused write landed");
    }

    /// A path is read as UTF-8 whatever script it is in, and bytes that
    /// are not UTF-8 are `EFAULT`, as is a range outside memory.
    #[test]
    fn a_path_reads_as_utf8_in_chinese_japanese_and_korean() {
        for path in ["/数据/文件.txt", "/データ/ファイル.txt", "/데이터/파일.txt"] {
            let mut backing = vec![0_u8; 128];
            backing[10..10 + path.len()].copy_from_slice(path.as_bytes());
            let memory = memory_over(&mut backing);
            assert_eq!(
                run(memory.read_path(GuestAddress::wasm32(10), path.len() as u32)),
                Ok(path.to_owned())
            );
        }
        let mut backing = vec![0xff_u8; 16];
        let memory = memory_over(&mut backing);
        assert_eq!(
            run(memory.read_path(GuestAddress::wasm32(0), 4)),
            Err(P1Stop::Errno(p1::errno::FAULT))
        );
        assert_eq!(
            run(memory.read_path(GuestAddress::wasm32(8), 16)),
            Err(P1Stop::Errno(p1::errno::FAULT))
        );
    }

    /// Bytes scatter over the buffers in order, each as far as they reach,
    /// and the count that went is written last.
    #[test]
    fn bytes_scatter_over_the_buffers_in_order() {
        let mut backing = vec![0_u8; 64];
        let memory = memory_over(&mut backing);
        let payload = "你好こんにちは안녕".as_bytes();
        run(memory.write_iovs_from_bytes(
            &[(0, 4), (10, 5), (20, 100)],
            payload,
            GuestAddress::wasm32(60),
        ))
        .expect("every buffer is in bounds");
        assert_eq!(backing[0..4], payload[0..4]);
        assert_eq!(backing[10..15], payload[4..9]);
        assert_eq!(backing[20..20 + payload.len() - 9], payload[9..]);
        assert_eq!(
            backing[60..64],
            (payload.len() as u32).to_le_bytes(),
            "the count of bytes that went"
        );
    }

    /// A buffer out of bounds stops the scatter with `EFAULT` and leaves
    /// the buffers before it written.
    #[test]
    fn a_scatter_stops_at_a_buffer_out_of_bounds() {
        let mut backing = vec![0_u8; 32];
        let memory = memory_over(&mut backing);
        assert_eq!(
            run(memory.write_iovs_from_bytes(
                &[(0, 4), (30, 8)],
                &[7; 12],
                GuestAddress::wasm32(16)
            )),
            Err(P1Stop::Errno(p1::errno::FAULT))
        );
        assert_eq!(backing[0..4], [7; 4]);
        assert_eq!(backing[16..20], [0; 4], "no count was reported");
    }

    #[test]
    fn an_empty_range_at_the_end_is_in_bounds() {
        let mut backing = vec![0_u8; 64];
        let memory = memory_over(&mut backing);
        assert_eq!(
            run(memory.read(GuestAddress::wasm32(64), 0, <[u8]>::len)),
            Ok(0)
        );
    }
}
