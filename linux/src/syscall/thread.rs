//! Thread creation (`clone`, `clone3`), thread-group exit tracking,
//! `sched_yield`, `sched_getaffinity`, `nanosleep` and `clock_nanosleep`.
//!
//! Flag rules and argument layouts follow Linux v6.12 (`kernel/fork.c`,
//! `kernel/sched/syscalls.c` and `kernel/time/posix-timers.c`).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU64, Ordering};

use super::futex::FUTEX_TID_MASK;
use super::*;

// include/uapi/linux/sched.h.
const CSIGNAL: u64 = 0x0000_00ff;
const CLONE_NEWTIME: u64 = 0x0000_0080;
const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_PIDFD: u64 = 0x0000_1000;
const CLONE_PTRACE: u64 = 0x0000_2000;
const CLONE_VFORK: u64 = 0x0000_4000;
const CLONE_PARENT: u64 = 0x0000_8000;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_SYSVSEM: u64 = 0x0004_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const CLONE_DETACHED: u64 = 0x0040_0000;
const CLONE_UNTRACED: u64 = 0x0080_0000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
const CLONE_NEWCGROUP: u64 = 0x0200_0000;
const CLONE_NEWUTS: u64 = 0x0400_0000;
const CLONE_NEWIPC: u64 = 0x0800_0000;
const CLONE_NEWUSER: u64 = 0x1000_0000;
const CLONE_NEWPID: u64 = 0x2000_0000;
const CLONE_NEWNET: u64 = 0x4000_0000;
const CLONE_IO: u64 = 0x8000_0000;
const CLONE_CLEAR_SIGHAND: u64 = 1 << 32;
const CLONE_INTO_CGROUP: u64 = 1 << 33;

const KNOWN_CLONE3_FLAGS: u64 = CLONE_NEWTIME
    | CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_PIDFD
    | CLONE_PTRACE
    | CLONE_VFORK
    | CLONE_PARENT
    | CLONE_THREAD
    | CLONE_NEWNS
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_UNTRACED
    | CLONE_CHILD_SETTID
    | CLONE_NEWCGROUP
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWNET
    | CLONE_IO
    | CLONE_CLEAR_SIGHAND
    | CLONE_INTO_CGROUP;

const REQUIRED_THREAD_FLAGS: u64 = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
const OPTIONAL_THREAD_FLAGS: u64 = CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_CHILD_SETTID
    | CLONE_DETACHED
    | CLONE_UNTRACED
    | CLONE_IO;

const CLONE_ARGS_SIZE_VER0: u64 = 64;
const CLONE_ARGS_SIZE_VER2: usize = 88;
const PAGE_BYTES: u64 = 4096;

// include/uapi/linux/time.h.
const TIMER_ABSTIME: u32 = 1;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

const ESRCH: u64 = 3;
const E2BIG: u64 = 7;
const EAGAIN: u64 = 11;

struct LiveThread {
    tid: NonZeroU32,
    robust_list: Arc<AtomicU64>,
}

/// Shared membership and termination state of one Linux thread group.
pub(super) struct ThreadGroup {
    next_tid: u32,
    live: Vec<LiveThread>,
    /// Closed once `exit_group`, a fatal signal, a fault or an owner kill
    /// condemns the thread group; no further threads are admitted.
    closed: bool,
    /// First group-wide termination (`exit_group`, fatal signal, fault or
    /// owner kill).
    group_termination: Option<ChildTermination>,
    /// Status passed to the most recent single-thread `exit(2)`, reported
    /// when the last thread exits without `exit_group`.
    last_thread_exit: u32,
}

impl ThreadGroup {
    pub(super) fn new(leader: NonZeroU32, robust_list: Arc<AtomicU64>) -> Self {
        let next_tid = leader
            .get()
            .checked_add(1)
            .filter(|&id| id <= FUTEX_TID_MASK)
            .unwrap_or(1);
        Self {
            next_tid,
            live: vec![LiveThread {
                tid: leader,
                robust_list,
            }],
            closed: false,
            group_termination: None,
            last_thread_exit: 0,
        }
    }

    pub(super) fn contains(&self, tid: u32) -> bool {
        self.live.iter().any(|thread| thread.tid.get() == tid)
    }

    pub(super) fn robust_list_of(&self, tid: u32) -> Option<u64> {
        self.live
            .iter()
            .find(|thread| thread.tid.get() == tid)
            .map(|thread| thread.robust_list.load(Ordering::Acquire))
    }

    pub(super) fn group_ended(&self) -> bool {
        self.closed
    }

    pub(super) fn all_exited(&self) -> bool {
        self.live.is_empty()
    }

    pub(super) fn termination(&self) -> ChildTermination {
        self.group_termination
            .unwrap_or(ChildTermination::Exited(self.last_thread_exit))
    }

    /// Admits a new thread into the group, enforcing `max_threads`
    /// (`RLIMIT_NPROC`) and allocating a unique nonzero TID within
    /// `FUTEX_TID_MASK`.
    pub(super) fn admit(
        &mut self,
        tgid: NonZeroU32,
        max_threads: u64,
        robust_list: Arc<AtomicU64>,
    ) -> Result<NonZeroU32, u64> {
        if self.closed || self.live.len() as u64 >= max_threads {
            return Err(EAGAIN);
        }
        let mut scanned = 0u32;
        while scanned < FUTEX_TID_MASK {
            let candidate = self.next_tid;
            self.next_tid = if candidate >= FUTEX_TID_MASK {
                1
            } else {
                candidate + 1
            };
            scanned += 1;
            if candidate == tgid.get() || self.contains(candidate) {
                continue;
            }
            let tid = NonZeroU32::new(candidate).expect("candidate is in 1..=FUTEX_TID_MASK");
            self.live.push(LiveThread { tid, robust_list });
            return Ok(tid);
        }
        Err(EAGAIN)
    }

    pub(super) fn withdraw(&mut self, tid: NonZeroU32) -> bool {
        if let Some(pos) = self.live.iter().position(|thread| thread.tid == tid) {
            self.live.swap_remove(pos);
        }
        self.live.is_empty()
    }

    /// Records a thread's exit and returns `(broadcast_group_stop, all_exited)`.
    pub(super) fn finish(&mut self, tid: NonZeroU32, exit: Exit) -> (bool, bool) {
        let broadcast = match exit {
            Exit::Thread(code) => {
                self.last_thread_exit = u32::from(code);
                false
            }
            Exit::Group(code) => {
                let first = !self.closed;
                self.closed = true;
                if self.group_termination.is_none() {
                    self.group_termination = Some(ChildTermination::Exited(u32::from(code)));
                }
                first
            }
        };
        let all_exited = self.withdraw(tid);
        (broadcast, all_exited)
    }

    /// Condemns the thread group with `termination` (first group termination
    /// wins) and withdraws `tid` if still present.
    pub(super) fn terminate_group(
        &mut self,
        tid: Option<NonZeroU32>,
        termination: ChildTermination,
    ) -> bool {
        let first = !self.closed;
        self.closed = true;
        if self.group_termination.is_none() {
            self.group_termination = Some(termination);
        }
        if let Some(tid) = tid {
            self.withdraw(tid);
        }
        first || self.live.is_empty()
    }
}

struct ParsedClone {
    flags: u64,
    stack: Option<u64>,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
}

fn validate_flag_combinations(flags: u64) -> Result<(), u64> {
    // kernel/fork.c `copy_process`.
    if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_SIGHAND != 0 && flags & CLONE_CLEAR_SIGHAND != 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_THREAD != 0 && flags & (CLONE_NEWUSER | CLONE_NEWPID | CLONE_PIDFD) != 0 {
        return Err(EINVAL);
    }
    if flags & (CLONE_NEWNS | CLONE_FS) == (CLONE_NEWNS | CLONE_FS) {
        return Err(EINVAL);
    }
    if flags & REQUIRED_THREAD_FLAGS != REQUIRED_THREAD_FLAGS {
        return Err(ENOSYS);
    }
    if flags & !(REQUIRED_THREAD_FLAGS | OPTIONAL_THREAD_FLAGS) != 0 {
        return Err(ENOSYS);
    }
    Ok(())
}

/// `clone` (`kernel/fork.c` `SYSCALL_DEFINE5(clone)`).
pub(super) fn clone<R: Resources, M: UserMemory, T: Thread<R>>(
    process: &Process<R>,
    args: [u64; 6],
    memory: &mut M,
    thread: &mut T,
) -> u64 {
    let decoded = process.shared.abi.clone_registers(args);
    // Legacy `clone` reads `lower_32_bits(clone_flags)` and keeps `CSIGNAL`
    // in the low byte, which `CLONE_THREAD` ignores.
    let flags = u64::from(decoded.flags as u32) & !CSIGNAL;
    if let Err(err) = validate_flag_combinations(flags) {
        return errno(err);
    }
    spawn_parsed_clone(
        process,
        ParsedClone {
            flags,
            stack: (decoded.stack != 0).then_some(decoded.stack),
            parent_tid: decoded.parent_tid,
            child_tid: decoded.child_tid,
            tls: decoded.tls,
        },
        memory,
        thread,
    )
}

/// `clone3(uargs, size)` (`kernel/fork.c` `SYSCALL_DEFINE2(clone3)` and
/// `copy_clone_args_from_user`).
pub(super) fn clone3<R: Resources, M: UserMemory, T: Thread<R>>(
    process: &Process<R>,
    args: [u64; 6],
    memory: &mut M,
    thread: &mut T,
) -> u64 {
    let [uargs, size, ..] = args;
    if size > PAGE_BYTES {
        return errno(E2BIG);
    }
    if size < CLONE_ARGS_SIZE_VER0 {
        return errno(EINVAL);
    }
    if uargs > process.shared.abi.task_size().saturating_sub(size) {
        return errno(EFAULT);
    }
    let known = (size as usize).min(CLONE_ARGS_SIZE_VER2);
    let mut raw = [0u8; CLONE_ARGS_SIZE_VER2];
    if memory.read(uargs, &mut raw[..known]).is_err() {
        return errno(EFAULT);
    }
    if size as usize > CLONE_ARGS_SIZE_VER2 {
        let extra = size as usize - CLONE_ARGS_SIZE_VER2;
        let mut tail = vec![0u8; extra];
        if memory
            .read(uargs + CLONE_ARGS_SIZE_VER2 as u64, &mut tail)
            .is_err()
        {
            return errno(EFAULT);
        }
        if tail.iter().any(|&byte| byte != 0) {
            return errno(E2BIG);
        }
    }
    let word =
        |index: usize| u64::from_le_bytes(raw[index * 8..(index + 1) * 8].try_into().unwrap());
    let flags = word(0);
    let child_tid = word(2);
    let parent_tid = word(3);
    let exit_signal = word(4);
    let stack_base = word(5);
    let stack_size = word(6);
    let tls = word(7);
    let set_tid = word(8);
    let set_tid_size = word(9);

    if flags & (CSIGNAL | CLONE_DETACHED) != 0
        || flags & !KNOWN_CLONE3_FLAGS != 0
        || exit_signal & !CSIGNAL != 0
        || (flags & (CLONE_THREAD | CLONE_PARENT) != 0 && exit_signal != 0)
        || (stack_base == 0) != (stack_size == 0)
        || (set_tid == 0) != (set_tid_size == 0)
    {
        return errno(EINVAL);
    }
    if set_tid_size != 0 {
        return errno(EPERM);
    }
    let stack = match stack_base {
        0 => None,
        base => match base.checked_add(stack_size) {
            Some(top) if top <= process.shared.abi.task_size() => Some(top),
            _ => return errno(EINVAL),
        },
    };
    if let Err(err) = validate_flag_combinations(flags) {
        return errno(err);
    }
    spawn_parsed_clone(
        process,
        ParsedClone {
            flags,
            stack,
            parent_tid,
            child_tid,
            tls,
        },
        memory,
        thread,
    )
}

fn write_tid<M: UserMemory>(address: u64, tid: u32, memory: &mut M) -> Result<(), MemoryFault> {
    if address & 3 == 0 {
        memory.atomic_store_u32(address, tid)
    } else {
        memory.write(address, &tid.to_le_bytes())
    }
}

fn spawn_parsed_clone<R: Resources, M: UserMemory, T: Thread<R>>(
    process: &Process<R>,
    parsed: ParsedClone,
    memory: &mut M,
    thread: &mut T,
) -> u64 {
    let tls = if parsed.flags & CLONE_SETTLS != 0 {
        if parsed.tls >= process.shared.abi.task_size() {
            return errno(EPERM);
        }
        Some(parsed.tls)
    } else {
        None
    };
    let max_threads = process.limit(Resource::Nproc).current;
    let robust_list = Arc::new(AtomicU64::new(0));
    let tid =
        match process
            .shared
            .threads
            .lock()
            .admit(process.pid(), max_threads, robust_list.clone())
        {
            Ok(tid) => tid,
            Err(err) => return errno(err),
        };
    if parsed.flags & CLONE_PARENT_SETTID != 0
        && write_tid(parsed.parent_tid, tid.get(), memory).is_err()
    {
        process.shared.threads.lock().withdraw(tid);
        return errno(EFAULT);
    }
    if parsed.flags & CLONE_CHILD_SETTID != 0 {
        let _ = write_tid(parsed.child_tid, tid.get(), memory);
    }
    let child = Process {
        shared: process.shared.clone(),
        tid,
        clear_child_tid: if parsed.flags & CLONE_CHILD_CLEARTID != 0 {
            parsed.child_tid
        } else {
            0
        },
        robust_list,
        exited: false,
    };
    match thread.spawn_thread(
        child,
        SpawnedThread {
            stack: parsed.stack,
            tls,
        },
    ) {
        Ok(()) => u64::from(tid.get()),
        Err(error) => {
            // `child` dropped inside `spawn_thread`, which already withdrew
            // `tid`; withdraw again idempotently in case a custom host dropped
            // after `exited` or before.
            process.shared.threads.lock().withdraw(tid);
            errno(match error {
                SpawnError::OutOfMemory => ENOMEM,
                SpawnError::Busy => EAGAIN,
                SpawnError::InvalidTls => EPERM,
            })
        }
    }
}

/// `sched_yield()` (`kernel/sched/syscalls.c`).
pub(super) async fn sched_yield() -> u64 {
    system::yield_now().await;
    0
}

/// `sched_getaffinity(pid, cpusetsize, mask)` (`kernel/sched/syscalls.c`).
///
/// Raw kernel syscall returns the byte length written to `mask` (a multiple
/// of `sizeof(unsigned long)`), which libc uses to zero any trailing bytes.
pub(super) fn sched_getaffinity<R: Resources, M: UserMemory, O: System>(
    process: &Process<R>,
    args: [u64; 6],
    memory: &mut M,
    output: &O,
) -> u64 {
    let pid = args[0] as u32 as i32;
    let len = args[1] as u32 as usize;
    let cpus = output.processor_count().max(1) as usize;
    let required = cpus.div_ceil(64) * 8;
    if !len.is_multiple_of(8) || len < required {
        return errno(EINVAL);
    }
    if pid < 0 || (pid != 0 && !process.shared.threads.lock().contains(pid as u32)) {
        return errno(ESRCH);
    }
    let mut mask = vec![0u8; required];
    for cpu in 0..cpus {
        mask[cpu / 8] |= 1 << (cpu % 8);
    }
    if memory.write(args[2], &mask).is_err() {
        return errno(EFAULT);
    }
    required as u64
}

/// `nanosleep(req, rem)` (`kernel/time/hrtimer.c`).
pub(super) async fn nanosleep<M: UserMemory, O: Clock + System>(
    args: [u64; 6],
    memory: &M,
    output: &O,
) -> u64 {
    clock_nanosleep([0, 0, args[0], args[1], 0, 0], memory, output).await
}

/// `clock_nanosleep(clockid, flags, req, rem)` (`kernel/time/posix-timers.c`).
pub(super) async fn clock_nanosleep<M: UserMemory, O: Clock + System>(
    args: [u64; 6],
    memory: &M,
    output: &O,
) -> u64 {
    enum SleepClock {
        Realtime,
        Monotonic,
    }
    let clock = match args[0] as u32 as i32 {
        0 | 11 => SleepClock::Realtime,
        1 | 2 | 7 => SleepClock::Monotonic,
        // `posix_cpu_nsleep` explicitly rejects `CLOCK_THREAD_CPUTIME_ID` with
        // `EINVAL`; clocks whose `k_clock.nsleep` is null answer `EOPNOTSUPP`.
        3 => return errno(EINVAL),
        4..=6 => return errno(EOPNOTSUPP),
        _ => return errno(EINVAL),
    };
    let mut bytes = [0u8; 16];
    if memory.read(args[2], &mut bytes).is_err() {
        return errno(EFAULT);
    }
    let sec = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(bytes[8..].try_into().unwrap());
    if sec < 0 || !(0..NANOS_PER_SECOND as i64).contains(&nsec) {
        return errno(EINVAL);
    }
    let flags = args[1] as u32;
    if flags & !TIMER_ABSTIME != 0 {
        return errno(EINVAL);
    }
    let requested = (sec as u64)
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(nsec as u64);
    if flags & TIMER_ABSTIME != 0 {
        let now = match clock {
            SleepClock::Realtime => output.realtime_nanos(),
            SleepClock::Monotonic => output.now_nanos(),
        };
        if requested > now {
            output
                .sleep_until(output.now_nanos().saturating_add(requested - now))
                .await;
        }
    } else if requested != 0 {
        output
            .sleep_until(output.now_nanos().saturating_add(requested))
            .await;
    }
    0
}
