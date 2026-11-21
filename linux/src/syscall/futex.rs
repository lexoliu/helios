//! Fast userspace mutexes (`futex`), robust futex lists (`set_robust_list`,
//! `get_robust_list`, `exit_robust_list`), and `clear_child_tid` release.
//!
//! Semantics and argument validation order follow Linux v6.12
//! (`kernel/futex/syscalls.c`, `kernel/futex/core.c`,
//! `kernel/futex/waitwake.c`, `kernel/futex/requeue.c`, and `kernel/fork.c`
//! `mm_release`).

use core::future::poll_fn;
use core::task::Poll;

use helios_unix::{
    FUTEX_BITSET_MATCH_ANY, FutexKey, FutexScope, FutexTable, GuestAddress, ProcessMemoryIdentity,
};

use super::*;

// include/uapi/linux/futex.h.
const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_PRIVATE_FLAG: u32 = 128;
const FUTEX_CLOCK_REALTIME: u32 = 256;

const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub(super) const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const ROBUST_LIST_LIMIT: usize = 2048;
const ROBUST_LIST_HEAD_BYTES: u64 = 24;
const FUTEX_ROBUST_MOD_MASK: u64 = 1;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const ESRCH: u64 = 3;
const EAGAIN: u64 = 11;
const ETIMEDOUT: u64 = 110;

/// Resolves `uaddr` to a futex queue key (`kernel/futex/core.c`
/// `get_futex_key`).
///
/// Alignment (`EINVAL`) is checked before the user-range check (`EFAULT`).
/// Private keys need no page-table walk unless the operation itself reads
/// `*uaddr`; shared keys validate that `uaddr` is mapped readable, as
/// `get_user_pages_fast` does on Linux.
fn futex_key<M: UserMemory>(
    abi: Abi,
    memory_id: ProcessMemoryIdentity,
    uaddr: u64,
    private: bool,
    memory: &M,
) -> Result<FutexKey, u64> {
    if uaddr & 3 != 0 {
        return Err(EINVAL);
    }
    if uaddr > abi.task_size().saturating_sub(4) {
        return Err(EFAULT);
    }
    let scope = if private {
        FutexScope::Private
    } else {
        memory.atomic_load_u32(uaddr).map_err(|_| EFAULT)?;
        FutexScope::Shared
    };
    Ok(FutexKey::with_scope(
        memory_id,
        GuestAddress::new(uaddr),
        scope,
    ))
}

fn read_timespec<M: UserMemory>(address: u64, memory: &M) -> Result<(u64, u64), u64> {
    let mut bytes = [0u8; 16];
    memory.read(address, &mut bytes).map_err(|_| EFAULT)?;
    let sec = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(bytes[8..].try_into().unwrap());
    if sec < 0 || !(0..NANOS_PER_SECOND as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    Ok((sec as u64, nsec as u64))
}

/// `futex(uaddr, op, val, utime, uaddr2, val3)` (`kernel/futex/syscalls.c`
/// `do_futex`).
pub(super) async fn futex<R: Resources, M: UserMemory, O: Clock + System>(
    process: &Process<R>,
    args: [u64; 6],
    memory: &M,
    output: &O,
) -> u64 {
    let op = args[1] as u32;
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if realtime && !matches!(cmd, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        return errno(ENOSYS);
    }
    let abi = process.shared.abi;
    let memory_id = process.shared.memory_id;
    let table = &process.shared.futex;

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let timeout = if args[3] != 0 {
                match read_timespec(args[3], memory) {
                    Ok((sec, nsec)) => {
                        Some(sec.saturating_mul(NANOS_PER_SECOND).saturating_add(nsec))
                    }
                    Err(err) => return errno(err),
                }
            } else {
                None
            };
            let bitset = if cmd == FUTEX_WAIT {
                FUTEX_BITSET_MATCH_ANY
            } else {
                args[5] as u32
            };
            if bitset == 0 {
                return errno(EINVAL);
            }
            let key = match futex_key(abi, memory_id, args[0], private, memory) {
                Ok(key) => key,
                Err(err) => return errno(err),
            };
            // Arm the wait before loading `*uaddr`, so no wake between the
            // load and parking can be missed.
            let mut registration = table.prepare_wait_bitset(key, bitset);
            let Ok(current) = memory.atomic_load_u32(args[0]) else {
                return errno(EFAULT);
            };
            if current != args[2] as u32 {
                return errno(EAGAIN);
            }
            let deadline = match timeout {
                None => None,
                Some(duration) if cmd == FUTEX_WAIT => {
                    if duration == 0 {
                        return if registration.is_released() {
                            0
                        } else {
                            errno(ETIMEDOUT)
                        };
                    }
                    Some(output.now_nanos().saturating_add(duration))
                }
                Some(target) => {
                    let now = if realtime {
                        output.realtime_nanos()
                    } else {
                        output.now_nanos()
                    };
                    if target <= now {
                        return if registration.is_released() {
                            0
                        } else {
                            errno(ETIMEDOUT)
                        };
                    }
                    Some(output.now_nanos().saturating_add(target - now))
                }
            };
            match deadline {
                None => {
                    registration.released().await;
                    0
                }
                Some(end) => {
                    {
                        let mut wake = core::pin::pin!(registration.released());
                        let mut sleep = core::pin::pin!(output.sleep_until(end));
                        poll_fn(|cx| {
                            if wake.as_mut().poll(cx).is_ready()
                                || sleep.as_mut().poll(cx).is_ready()
                            {
                                Poll::Ready(())
                            } else {
                                Poll::Pending
                            }
                        })
                        .await;
                    }
                    if registration.is_released() {
                        0
                    } else {
                        errno(ETIMEDOUT)
                    }
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if cmd == FUTEX_WAKE {
                FUTEX_BITSET_MATCH_ANY
            } else {
                args[5] as u32
            };
            if bitset == 0 {
                return errno(EINVAL);
            }
            let key = match futex_key(abi, memory_id, args[0], private, memory) {
                Ok(key) => key,
                Err(err) => return errno(err),
            };
            let nr_wake = args[2] as u32 as i32;
            if nr_wake <= 0 {
                return 0;
            }
            table.wake_bitset(key, nr_wake as usize, bitset) as u64
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let nr_wake = args[2] as u32 as i32;
            let nr_requeue = args[3] as u32 as i32;
            if nr_wake < 0 || nr_requeue < 0 {
                return errno(EINVAL);
            }
            let source = match futex_key(abi, memory_id, args[0], private, memory) {
                Ok(key) => key,
                Err(err) => return errno(err),
            };
            let target = match futex_key(abi, memory_id, args[4], private, memory) {
                Ok(key) => key,
                Err(err) => return errno(err),
            };
            let expected = (cmd == FUTEX_CMP_REQUEUE).then_some(args[5] as u32);
            let outcome = table.requeue_if(
                source,
                target,
                nr_wake as usize,
                nr_requeue as usize,
                || match expected {
                    None => Ok(()),
                    Some(val3) => {
                        let current = memory.atomic_load_u32(args[0]).map_err(|_| EFAULT)?;
                        if current != val3 {
                            return Err(EAGAIN);
                        }
                        Ok(())
                    }
                },
            );
            // Both `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE` delegate to
            // `futex_requeue` (`kernel/futex/requeue.c`), which returns
            // `task_count` (woken + requeued).
            match outcome {
                Ok(outcome) => (outcome.woken + outcome.requeued) as u64,
                Err(err) => errno(err),
            }
        }
        _ => errno(ENOSYS),
    }
}

/// `set_robust_list(head, len)` (`kernel/futex/syscalls.c`).
pub(super) fn set_robust_list<R: Resources>(process: &Process<R>, args: [u64; 6]) -> u64 {
    if args[1] != ROBUST_LIST_HEAD_BYTES {
        return errno(EINVAL);
    }
    process
        .robust_list
        .store(args[0], core::sync::atomic::Ordering::Release);
    0
}

/// `get_robust_list(pid, head_ptr, len_ptr)` (`kernel/futex/syscalls.c`).
pub(super) fn get_robust_list<R: Resources, M: UserMemory>(
    process: &Process<R>,
    args: [u64; 6],
    memory: &mut M,
) -> u64 {
    let pid = args[0] as u32 as i32;
    if pid < 0 {
        return errno(ESRCH);
    }
    let head = if pid == 0 || pid as u32 == process.tid.get() {
        process
            .robust_list
            .load(core::sync::atomic::Ordering::Acquire)
    } else {
        let Some(head) = process.shared.threads.lock().robust_list_of(pid as u32) else {
            return errno(ESRCH);
        };
        head
    };
    if memory
        .write(args[2], &ROBUST_LIST_HEAD_BYTES.to_le_bytes())
        .is_err()
    {
        return errno(EFAULT);
    }
    if memory.write(args[1], &head.to_le_bytes()).is_err() {
        return errno(EFAULT);
    }
    0
}

/// Cleans up a dying thread's robust futex list and clears/wakes its
/// `clear_child_tid` (`kernel/exit.c` `do_exit` -> `mm_release`).
pub(super) fn exit_thread<R: Resources, M: UserMemory>(process: &Process<R>, memory: &mut M) {
    let head = process
        .robust_list
        .swap(0, core::sync::atomic::Ordering::AcqRel);
    if head != 0 {
        exit_robust_list(
            head,
            process.tid.get(),
            process.shared.memory_id,
            &process.shared.futex,
            memory,
        );
    }
    if process.clear_child_tid != 0 {
        let address = process.clear_child_tid;
        if address & 3 == 0 {
            let _ = memory.atomic_store_u32(address, 0);
            process.shared.futex.wake(
                FutexKey::shared(process.shared.memory_id, GuestAddress::new(address)),
                1,
            );
        } else {
            let _ = memory.write(address, &0u32.to_le_bytes());
        }
    }
}

/// Walks a dying thread's `struct robust_list_head` (`kernel/futex/core.c`
/// `exit_robust_list`).
fn exit_robust_list<M: UserMemory>(
    head: u64,
    tid: u32,
    memory_id: ProcessMemoryIdentity,
    table: &FutexTable,
    memory: &mut M,
) {
    let mut header = [0u8; 24];
    if memory.read(head, &mut header).is_err() {
        return;
    }
    let mut entry = u64::from_le_bytes(header[..8].try_into().unwrap());
    let futex_offset = i64::from_le_bytes(header[8..16].try_into().unwrap());
    let pending = u64::from_le_bytes(header[16..24].try_into().unwrap());

    let mut count = 0;
    while (entry & !FUTEX_ROBUST_MOD_MASK) != head {
        let current = entry & !FUTEX_ROBUST_MOD_MASK;
        let mut next_bytes = [0u8; 8];
        // Read `next` before waking a waiter that may reuse or unmap the lock.
        if memory.read(current, &mut next_bytes).is_err() {
            return;
        }
        let next_entry = u64::from_le_bytes(next_bytes);
        if current != (pending & !FUTEX_ROBUST_MOD_MASK)
            && handle_futex_death(
                current.wrapping_add(futex_offset as u64),
                tid,
                false,
                memory_id,
                table,
                memory,
            )
            .is_err()
        {
            return;
        }
        entry = next_entry;
        count += 1;
        if count >= ROBUST_LIST_LIMIT {
            break;
        }
    }
    if pending != 0 {
        let pending_addr = (pending & !FUTEX_ROBUST_MOD_MASK).wrapping_add(futex_offset as u64);
        let _ = handle_futex_death(pending_addr, tid, true, memory_id, table, memory);
    }
}

/// Marks a robust futex `FUTEX_OWNER_DIED` when owned by `tid` and wakes one
/// shared waiter if `FUTEX_WAITERS` was set (`kernel/futex/core.c`
/// `handle_futex_death`).
fn handle_futex_death<M: UserMemory>(
    uaddr: u64,
    tid: u32,
    pending_op: bool,
    memory_id: ProcessMemoryIdentity,
    table: &FutexTable,
    memory: &mut M,
) -> Result<(), ()> {
    if uaddr & 3 != 0 {
        return Err(());
    }
    loop {
        let uval = memory.atomic_load_u32(uaddr).map_err(|_| ())?;
        let owner = uval & FUTEX_TID_MASK;
        if pending_op && owner == 0 {
            return Ok(());
        }
        if owner != tid {
            return Ok(());
        }
        let nval = (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        match memory
            .atomic_compare_exchange_u32(uaddr, uval, nval)
            .map_err(|_| ())?
        {
            Ok(_) => {
                if uval & FUTEX_WAITERS != 0 {
                    table.wake(FutexKey::shared(memory_id, GuestAddress::new(uaddr)), 1);
                }
                return Ok(());
            }
            Err(_) => continue,
        }
    }
}
