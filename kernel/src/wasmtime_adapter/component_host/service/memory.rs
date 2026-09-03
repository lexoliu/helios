use super::*;

pub(super) struct Preview1IovLayout {
    pub(super) iovs: Preview1Iovs,
    pub(super) byte_len: usize,
}

impl Preview1IovLayout {
    pub(super) fn byte_len_u32(&self) -> Result<u32, i32> {
        u32::try_from(self.byte_len).map_err(|_| p1::errno::OVERFLOW)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Preview1Memory {
    pub(super) base: usize,
    pub(super) len: usize,
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
    path: u32,
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

pub(super) fn p1_write_string_array<'a, CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pointers: u32,
    buffer: u32,
    values: impl Iterator<Item = &'a str>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let mut current = buffer;
    let mut status = p1::errno::SUCCESS;
    for (index, value) in values.enumerate() {
        let pointer = match u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(4))
            .and_then(|offset| pointers.checked_add(offset))
        {
            Some(pointer) => pointer,
            None => return p1::errno::OVERFLOW,
        };
        status = status.max(p1_write_u32(caller, memory, pointer, current));
        status = status.max(p1_write_memory(caller, memory, current, value.as_bytes()));
        current = match current
            .checked_add(u32::try_from(value.len()).unwrap_or(u32::MAX))
            .and_then(|value| value.checked_add(1))
        {
            Some(next) => next,
            None => return p1::errno::OVERFLOW,
        };
        status = status.max(p1_write_u8(caller, memory, current - 1, 0));
    }
    status
}

pub(super) fn p1_memory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
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

pub(super) fn p1_memory_from_instance<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
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
    ptr: u32,
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
    ptr: u32,
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
    ptr: u32,
    len: usize,
) -> Result<usize, ProgramExecError> {
    let start = ptr as usize;
    let end = start.checked_add(len).ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
    })?;
    if end > memory.len {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    Ok(start)
}

pub(super) fn preview1_write_memory(memory: Preview1Memory, ptr: u32, bytes: &[u8]) -> i32 {
    let start = ptr as usize;
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

pub(super) fn preview1_read_u32(memory: Preview1Memory, ptr: u32) -> Result<u32, ProgramExecError> {
    let bytes = preview1_read_memory(memory, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("Preview1 raw u32 read must return exactly 4 bytes")
    })))
}

pub(super) fn preview1_write_u32(memory: Preview1Memory, ptr: u32, value: u32) -> i32 {
    preview1_write_memory(memory, ptr, &value.to_le_bytes())
}

// Preview1 socket/file profiles charge iov decoding separately from payload
// copy. Keep length accounting in the guest-table decode pass so hot read and
// recv syscalls do not rescan the same SmallVec before touching the real I/O
// path.
pub(super) fn p1_read_iovs_with_byte_len<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: u32,
    iovs_len: u32,
) -> Result<Preview1IovLayout, i32> {
    let mut result = Preview1Iovs::new();
    let mut byte_len = 0usize;
    for index in 0..iovs_len {
        let offset = index.checked_mul(8).ok_or(p1::errno::OVERFLOW)?;
        let iov = iovs.checked_add(offset).ok_or(p1::errno::OVERFLOW)?;
        let ptr = p1_try_read_u32(caller, memory, iov).map_err(|_| p1::errno::FAULT)?;
        let len = p1_try_read_u32(caller, memory, iov + 4).map_err(|_| p1::errno::FAULT)?;
        let len_usize = usize::try_from(len).map_err(|_| p1::errno::OVERFLOW)?;
        byte_len = byte_len.checked_add(len_usize).ok_or(p1::errno::OVERFLOW)?;
        result.push((ptr, len));
    }
    Ok(Preview1IovLayout {
        iovs: result,
        byte_len,
    })
}

pub(super) fn p1_read_iovs_to_bytes<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: u32,
    iovs_len: u32,
) -> Result<Vec<u8>, i32> {
    let layout = p1_read_iovs_with_byte_len(caller, memory, iovs, iovs_len)?;
    let mut bytes: Vec<u8> = Vec::with_capacity(layout.byte_len);
    let mut written = 0usize;
    for (ptr, len) in layout.iovs {
        let len = usize::try_from(len).map_err(|_| p1::errno::OVERFLOW)?;
        let start = preview1_memory_start(memory, ptr, len).map_err(|_| p1::errno::FAULT)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                (memory.base as *const u8).add(start),
                bytes.as_mut_ptr().add(written),
                len,
            );
        }
        written = written.checked_add(len).ok_or(p1::errno::OVERFLOW)?;
    }
    unsafe {
        bytes.set_len(written);
    }
    Ok(bytes)
}

pub(super) fn p1_iovs_memory_ranges(
    memory: Preview1Memory,
    iovs: &[(u32, u32)],
) -> Result<Preview1IovRanges, i32> {
    let mut ranges = Preview1IovRanges::with_capacity(iovs.len());
    for (ptr, len) in iovs {
        let len = usize::try_from(*len).map_err(|_| p1::errno::OVERFLOW)?;
        let start = preview1_memory_start(memory, *ptr, len).map_err(|_| p1::errno::FAULT)?;
        ranges.push((start, len));
    }
    Ok(ranges)
}

pub(super) fn copy_preview1_ranges_to_slice(
    memory_base: *const u8,
    ranges: &[(usize, usize)],
    destination: &mut [u8],
) {
    let mut copied = 0usize;
    for (start, len) in ranges {
        let next = copied
            .checked_add(*len)
            .expect("validated preview1 iov ranges overflowed while copying");
        // SAFETY: `p1_iovs_memory_ranges` validated every source range
        // against the live guest memory, and `destination` was allocated
        // to the exact summed iov length before this copy.
        unsafe {
            core::ptr::copy_nonoverlapping(
                memory_base.add(*start),
                destination.as_mut_ptr().add(copied),
                *len,
            );
        }
        copied = next;
    }
    assert_eq!(
        copied,
        destination.len(),
        "validated preview1 iov ranges did not fill destination"
    );
}

pub(super) fn p1_read_memory<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    len: usize,
) -> Result<Vec<u8>, ProgramExecError> {
    preview1_read_memory(memory, ptr, len)
}

pub(super) fn p1_read_memory_into<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    bytes: &mut [u8],
) -> Result<(), ProgramExecError> {
    preview1_read_memory_into(memory, ptr, bytes)
}

pub(super) fn p1_write_memory<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    bytes: &[u8],
) -> i32 {
    preview1_write_memory(memory, ptr, bytes)
}

pub(super) fn p1_try_read_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u32, ProgramExecError> {
    let mut bytes = [0_u8; 4];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(super) fn p1_try_read_u8<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u8, ProgramExecError> {
    let mut bytes = [0_u8; 1];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(bytes[0])
}

pub(super) fn p1_try_read_u16<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u16, ProgramExecError> {
    let mut bytes = [0_u8; 2];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

pub(super) fn p1_try_read_u64<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u64, ProgramExecError> {
    let mut bytes = [0_u8; 8];
    p1_read_memory_into(caller, memory, ptr, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub(super) fn p1_write_u8<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u8,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &[value])
}

pub(super) fn p1_write_u16<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u16,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

pub(super) fn p1_write_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u32,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

pub(super) fn p1_write_u64<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u64,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

pub(super) fn p1_write_iovs_from_bytes<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: Preview1Iovs,
    bytes: &[u8],
    written_out: u32,
) -> i32 {
    let mut copied = 0usize;
    for (ptr, len) in iovs {
        if copied >= bytes.len() {
            break;
        }
        let len = (len as usize).min(bytes.len() - copied);
        let status = p1_write_memory(caller, memory, ptr, &bytes[copied..copied + len]);
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
