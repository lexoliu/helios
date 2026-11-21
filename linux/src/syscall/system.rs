//! Clocks, entropy, identity, resource limits and the machine's
//! description.
//!
//! Every call here is decoded identically on x86-64 and aarch64: the
//! structures they exchange (`struct timespec`, `struct timeval`,
//! `struct rlimit64`, `struct sysinfo`, `struct new_utsname`) have one
//! LP64 layout in both ABIs (uapi/linux/time.h, uapi/linux/resource.h,
//! uapi/linux/sysinfo.h, uapi/linux/utsname.h, with `__kernel_long_t` from
//! uapi/asm-generic/posix_types.h), and `gid_t`/`uid_t` are 32 bits in
//! both. Only the call numbers differ, in [`crate::abi`]. The semantics
//! cited in each function are those of Linux v6.12.
//!
//! The one call that reaches a resource is `clock_settime`: the platform's
//! [`System::set_realtime_nanos`] refuses it unless the process's authority
//! holds the right WASIX's `clock_time_set` needs. Identity values are
//! reported, never authority, and the limits a process may raise are those
//! Linux lets an unprivileged process raise: no right of a process's
//! authority stands for `CAP_SYS_RESOURCE` or `CAP_SYS_NICE`.

use super::*;
use alloc::vec::Vec;
use core::future::poll_fn;
use core::task::Poll;
use helios_unix::DescriptorLimit;

const ESRCH: u64 = 3;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// What the platform keeps that these calls report or change. A process
/// calls it only while dispatching one of its own system calls.
pub trait System {
    /// `CLOCK_REALTIME`: nanoseconds since the Unix epoch, as this process
    /// sees them.
    fn realtime_nanos(&self) -> u64;

    /// Sets this process's view of `CLOCK_REALTIME`. Refused when the
    /// process's authority lacks the right to set the wall clock.
    fn set_realtime_nanos(&mut self, nanos: u64) -> Result<(), Denied>;

    /// Nanoseconds the calling thread has been executing: what
    /// `CLOCK_THREAD_CPUTIME_ID` reads.
    fn cpu_nanos(&self) -> u64;

    /// Nanoseconds every thread of the process has been executing: what
    /// `CLOCK_PROCESS_CPUTIME_ID` reads.
    fn process_cpu_nanos(&self) -> u64 {
        self.cpu_nanos()
    }

    /// Online processors available to this process (`sched_getaffinity`).
    fn processor_count(&self) -> u32 {
        1
    }

    /// Fills `output` from the kernel's entropy. Never blocks: the pool is
    /// seeded before any process starts.
    fn fill_random(&mut self, output: &mut [u8]);

    /// Machine memory, in bytes.
    fn memory(&self) -> MemoryTotals;

    /// How many processes the machine runs now.
    fn process_count(&self) -> u32;

    /// What `uname` reports as the kernel's build, up to 64 bytes.
    fn kernel_version(&self) -> &str;
}

/// The machine's memory as `sysinfo` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryTotals {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// The Linux release whose UAPI this crate decodes: every number and layout
/// cited here is that release's, so it is what `uname` reports.
pub const RELEASE: &str = "6.12.0";

// Linux init/Kconfig `CONFIG_DEFAULT_HOSTNAME` and include/linux/uts.h
// `UTS_DOMAINNAME`: what a machine reports before either is set. Helios
// has neither a host name nor a domain name to set.
const UNSET_NAME: &str = "(none)";

/// Linux's own ceiling on `RLIMIT_NOFILE`'s hard value: the default of the
/// `fs.nr_open` sysctl (fs/file.c `sysctl_nr_open`).
pub const NR_OPEN_DEFAULT: u64 = 1024 * 1024;

// uapi/linux/fs.h: the soft limit a process starts with and the hard limit
// its table of descriptors is bounded by when nothing raised it.
const INR_OPEN_MAX: u64 = 4096;

// uapi/linux/resource.h `_STK_LIM`, `MLOCK_LIMIT` and `MQ_BYTES_MAX`
// (include/uapi/linux/mqueue.h), which INIT_RLIMITS (asm-generic/resource.h)
// starts the init task with.
const STACK_LIMIT: u64 = 8 * 1024 * 1024;
const MEMLOCK_LIMIT: u64 = 8 * 1024 * 1024;
const MSGQUEUE_LIMIT: u64 = 819_200;

// kernel/fork.c `set_max_threads`: the thread count is bounded so thread
// structures take an eighth of memory at THREAD_SIZE (16 KiB on x86-64 and
// arm64), within MIN_THREADS and MAX_THREADS (FUTEX_TID_MASK).
const THREAD_SIZE: u64 = 16 * 1024;
const MIN_THREADS: u64 = 20;
const MAX_THREADS: u64 = 0x3fff_ffff;

/// `RLIM_INFINITY`: uapi/asm-generic/resource.h.
pub const RLIM_INFINITY: u64 = !0;

/// A resource limit, soft and hard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rlimit {
    pub current: u64,
    pub maximum: u64,
}

impl Rlimit {
    const UNLIMITED: Self = Self {
        current: RLIM_INFINITY,
        maximum: RLIM_INFINITY,
    };
}

/// A resource `prlimit64` names, numbered as uapi/asm-generic/resource.h
/// numbers it (identical on x86-64 and aarch64).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Fsize,
    Data,
    Stack,
    Core,
    Rss,
    Nproc,
    Nofile,
    Memlock,
    As,
    Locks,
    Sigpending,
    Msgqueue,
    Nice,
    Rtprio,
    Rttime,
}

impl Resource {
    /// In Linux's numbering order, so a resource's number is its index.
    const ALL: [Self; 16] = [
        Self::Cpu,
        Self::Fsize,
        Self::Data,
        Self::Stack,
        Self::Core,
        Self::Rss,
        Self::Nproc,
        Self::Nofile,
        Self::Memlock,
        Self::As,
        Self::Locks,
        Self::Sigpending,
        Self::Msgqueue,
        Self::Nice,
        Self::Rtprio,
        Self::Rttime,
    ];

    /// The resource Linux numbers `number`; `None` from `RLIM_NLIMITS` up.
    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL.get(number as usize).copied()
    }
}

/// A process's real, effective and saved ID of one kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ids {
    pub real: u32,
    pub effective: u32,
    pub saved: u32,
}

/// The user and group IDs a process reports. They are values the program
/// reads and nothing more: no call here, and no later call, treats one as
/// permission to reach a resource.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub user: Ids,
    pub group: Ids,
    pub supplementary_groups: Vec<u32>,
}

/// What a process starts with that its launcher decides.
#[derive(Clone)]
pub struct Start {
    pub credentials: Credentials,
    /// The machine's memory, which sizes `RLIMIT_NPROC` and
    /// `RLIMIT_SIGPENDING` as Linux's `fork_init` does.
    pub memory_bytes: u64,
    /// The highest hard `RLIMIT_NOFILE` a process may set: the operator's
    /// `fs.nr_open`.
    pub open_files_ceiling: u64,
    /// Shared futex wait table for this process's address space.
    pub futex: helios_unix::FutexTable,
}

impl Default for Start {
    fn default() -> Self {
        Self {
            credentials: Credentials::default(),
            memory_bytes: 0,
            open_files_ceiling: NR_OPEN_DEFAULT,
            futex: helios_unix::FutexTable::new(),
        }
    }
}

/// The process-wide state these calls read and change, apart from the
/// descriptor table whose limit is `RLIMIT_NOFILE`'s soft value.
#[derive(Clone, Debug)]
pub(super) struct Attributes {
    pub(super) credentials: Credentials,
    values: [Rlimit; 16],
    open_files_ceiling: u64,
    umask: u32,
    nice: i32,
}

impl Attributes {
    pub(super) fn new(start: Start, open_files: DescriptorLimit) -> Self {
        // asm-generic/resource.h INIT_RLIMITS.
        let mut values = [Rlimit::UNLIMITED; 16];
        let max_threads = (start.memory_bytes / (THREAD_SIZE * 8)).clamp(MIN_THREADS, MAX_THREADS);
        let threads = Rlimit {
            current: max_threads / 2,
            maximum: max_threads / 2,
        };
        let open_files = open_files.get() as u64;
        values[Resource::Stack as usize] = Rlimit {
            current: STACK_LIMIT,
            maximum: RLIM_INFINITY,
        };
        values[Resource::Core as usize] = Rlimit {
            current: 0,
            maximum: RLIM_INFINITY,
        };
        values[Resource::Nproc as usize] = threads;
        values[Resource::Nofile as usize] = Rlimit {
            current: open_files,
            maximum: INR_OPEN_MAX.max(open_files),
        };
        values[Resource::Memlock as usize] = Rlimit {
            current: MEMLOCK_LIMIT,
            maximum: MEMLOCK_LIMIT,
        };
        values[Resource::Sigpending as usize] = threads;
        values[Resource::Msgqueue as usize] = Rlimit {
            current: MSGQUEUE_LIMIT,
            maximum: MSGQUEUE_LIMIT,
        };
        values[Resource::Nice as usize] = Rlimit {
            current: 0,
            maximum: 0,
        };
        values[Resource::Rtprio as usize] = Rlimit {
            current: 0,
            maximum: 0,
        };
        Self {
            credentials: start.credentials,
            values,
            open_files_ceiling: start.open_files_ceiling,
            // fs/fs_struct.c `init_fs`.
            umask: 0o022,
            nice: 0,
        }
    }
}

/// Linux maps a nice value onto the one `RLIMIT_NICE` and `getpriority`
/// count in: `20 - nice`, from 40 for -20 down to 1 for 19
/// (include/linux/sched/prio.h `nice_to_rlimit`).
fn nice_to_rlimit(nice: i32) -> u64 {
    (20 - nice) as u64
}

impl<R: Resources> Process<R> {
    /// The limit of `resource`. `RLIMIT_NOFILE`'s soft value is the
    /// descriptor table's own limit, so a table whose owner sets it and a
    /// program that sets it through a call agree.
    pub fn limit(&self, resource: Resource) -> Rlimit {
        let mut limit = self.shared.attributes.lock().values[resource as usize];
        if resource == Resource::Nofile {
            limit.current = self.shared.descriptors.lock().limit.get() as u64;
        }
        limit
    }

    /// kernel/sys.c `do_prlimit`: the old limit, after `new` replaced it.
    fn set_limit(&mut self, resource: u32, new: Option<Rlimit>) -> Result<Rlimit, u64> {
        let resource = Resource::from_number(resource).ok_or(EINVAL)?;
        let mut attributes = self.shared.attributes.lock();
        let mut old = attributes.values[resource as usize];
        if resource == Resource::Nofile {
            old.current = self.shared.descriptors.lock().limit.get() as u64;
        }
        let Some(new) = new else { return Ok(old) };
        if new.current > new.maximum {
            return Err(EINVAL);
        }
        if resource == Resource::Nofile && new.maximum > attributes.open_files_ceiling {
            return Err(EPERM);
        }
        // Raising a hard limit is `CAP_SYS_RESOURCE`'s, which no right of a
        // process's authority names: the refusal an unprivileged Linux
        // process meets.
        if new.maximum > old.maximum {
            return Err(EPERM);
        }
        attributes.values[resource as usize] = new;
        if resource == Resource::Nofile {
            // The soft value is the table's; bounded by the ceiling above.
            self.shared.descriptors.lock().limit = DescriptorLimit::new(new.current as usize);
        }
        Ok(old)
    }

    /// Whether `pid` names this process or one of its threads: 0 is the
    /// caller.
    fn names_self(&self, pid: i32) -> bool {
        pid == 0
            || u64::try_from(pid).is_ok_and(|pid| {
                pid == self.shared.identity.id().get()
                    || u32::try_from(pid).is_ok_and(|tid| self.shared.threads.lock().contains(tid))
            })
    }
}

fn timespec(nanos: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&(nanos / NANOS_PER_SECOND).to_le_bytes());
    bytes[8..].copy_from_slice(&(nanos % NANOS_PER_SECOND).to_le_bytes());
    bytes
}

fn write_result<M: UserMemory>(memory: &mut M, address: u64, bytes: &[u8]) -> u64 {
    match memory.write(address, bytes) {
        Ok(()) => 0,
        Err(_) => errno(EFAULT),
    }
}

/// What a `clockid_t` reads, or `None` where Linux has no clock of that
/// number the kernel can serve (kernel/time/posix-timers.c `posix_clocks`).
/// The alarm clocks answer `EINVAL` without a wake-capable real-time clock
/// device, which Helios does not have; the auxiliary clocks (`CLOCK_AUX`,
/// 16 to 23) answer as on a kernel built without `CONFIG_POSIX_AUX_CLOCKS`,
/// whose table ends at `CLOCK_TAI`; and a negative ID names a CPU-time
/// clock of a process or a dynamic POSIX clock, neither of which exists
/// here.
enum ClockSource {
    Realtime,
    Monotonic,
    ProcessCpuTime,
    ThreadCpuTime,
}

fn clock_source(id: u64) -> Option<ClockSource> {
    // uapi/linux/time.h CLOCK_*; the argument is an `int`.
    match id as u32 as i32 {
        0 | 5 | 11 => Some(ClockSource::Realtime), // REALTIME, REALTIME_COARSE, TAI
        1 | 4 | 6 | 7 => Some(ClockSource::Monotonic), // MONOTONIC, _RAW, _COARSE, BOOTTIME
        2 => Some(ClockSource::ProcessCpuTime),    // PROCESS_CPUTIME_ID
        3 => Some(ClockSource::ThreadCpuTime),     // THREAD_CPUTIME_ID
        _ => None,
    }
}

pub(super) fn clock_gettime<M: UserMemory, O: Clock + System>(
    args: [u64; 6],
    memory: &mut M,
    output: &O,
) -> u64 {
    let nanos = match clock_source(args[0]) {
        None => return errno(EINVAL),
        // CLOCK_TAI differs from the real-time clock by the TAI offset,
        // which is zero until something sets it, and nothing here does.
        Some(ClockSource::Realtime) => output.realtime_nanos(),
        Some(ClockSource::Monotonic) => output.now_nanos(),
        Some(ClockSource::ProcessCpuTime) => output.process_cpu_nanos(),
        Some(ClockSource::ThreadCpuTime) => output.cpu_nanos(),
    };
    write_result(memory, args[1], &timespec(nanos))
}

pub(super) fn clock_getres<M: UserMemory>(args: [u64; 6], memory: &mut M) -> u64 {
    if clock_source(args[0]).is_none() {
        return errno(EINVAL);
    }
    // A null `tp` asks only whether the clock exists (clock_getres(2)).
    if args[1] == 0 {
        return 0;
    }
    // Every clock here is read at the platform's full tick resolution and
    // answers hrtimer_resolution, which Linux reports as 1 ns with
    // high-resolution timers; the coarse clocks are read as finely as the
    // others.
    write_result(memory, args[1], &timespec(1))
}

pub(super) fn clock_settime<M: UserMemory, O: System>(
    args: [u64; 6],
    memory: &mut M,
    output: &mut O,
) -> u64 {
    // Only CLOCK_REALTIME (0) has a setter (`clock_set`); every other
    // clock answers EINVAL, as does one that does not exist.
    if args[0] as u32 != 0 {
        return errno(EINVAL);
    }
    let mut bytes = [0; 16];
    if memory.read(args[1], &mut bytes).is_err() {
        return errno(EFAULT);
    }
    let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let nanoseconds = u64::from_le_bytes(bytes[8..].try_into().unwrap());
    // include/linux/time64.h `timespec64_valid_settod`: no date before the
    // epoch, a normalized nanosecond count, and a second count that leaves
    // thirty years of uptime before ktime_t overflows
    // (`TIME_SETTOD_SEC_MAX`).
    const SETTOD_SECONDS_MAX: i64 = (i64::MAX / NANOS_PER_SECOND as i64) - 30 * 365 * 24 * 3600;
    if seconds < 0 || nanoseconds >= NANOS_PER_SECOND || seconds >= SETTOD_SECONDS_MAX {
        return errno(EINVAL);
    }
    let nanos = seconds as u64 * NANOS_PER_SECOND + nanoseconds;
    match output.set_realtime_nanos(nanos) {
        Ok(()) => 0,
        // cap_settime: `CAP_SYS_TIME`.
        Err(Denied) => errno(EPERM),
    }
}

pub(super) fn gettimeofday<M: UserMemory, O: System>(
    args: [u64; 6],
    memory: &mut M,
    output: &O,
) -> u64 {
    if args[0] != 0 {
        let nanos = output.realtime_nanos();
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&(nanos / NANOS_PER_SECOND).to_le_bytes());
        bytes[8..].copy_from_slice(&(nanos % NANOS_PER_SECOND / 1000).to_le_bytes());
        if memory.write(args[0], &bytes).is_err() {
            return errno(EFAULT);
        }
    }
    if args[1] != 0 {
        // `sys_tz`: zero minutes west and no daylight-saving correction
        // until something sets the zone, which no call here does.
        return write_result(memory, args[1], &[0; 8]);
    }
    0
}

pub(super) fn time<M: UserMemory, O: System>(args: [u64; 6], memory: &mut M, output: &O) -> u64 {
    let seconds = output.realtime_nanos() / NANOS_PER_SECOND;
    if args[0] != 0 && memory.write(args[0], &seconds.to_le_bytes()).is_err() {
        return errno(EFAULT);
    }
    seconds
}

/// Yields to the executor once, so a long call does not hold its processor
/// the way Linux's `cond_resched` keeps a long `getrandom` from doing.
pub(super) async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

pub(super) async fn getrandom<M: UserMemory, O: System>(
    args: [u64; 6],
    memory: &mut M,
    output: &mut O,
) -> u64 {
    // uapi/linux/random.h: GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE.
    const NONBLOCK_RANDOM_INSECURE: u32 = 1 | 2 | 4;
    const RANDOM: u32 = 2;
    const INSECURE: u32 = 4;
    let flags = args[2] as u32;
    if flags & !NONBLOCK_RANDOM_INSECURE != 0 || flags & (RANDOM | INSECURE) == RANDOM | INSECURE {
        return errno(EINVAL);
    }
    // The pool is seeded before a process starts, so GRND_NONBLOCK never
    // meets an unready one, and GRND_RANDOM and GRND_INSECURE read the
    // same generator as plain getrandom has since Linux 5.6.
    // `import_ubuf` bounds the count at MAX_RW_COUNT.
    let count = args[1].min(MAX_RW_COUNT);
    if args[0].checked_add(count).is_none() {
        return errno(EFAULT);
    }
    // drivers/char/random.c `get_random_bytes_user` copies a page at a
    // time and gives the processor up between pages.
    const PAGES_PER_TURN: u64 = 16;
    let mut buffer = [0; 4096];
    let mut written = 0_u64;
    while written < count {
        let address = args[0] + written;
        let page_remaining = buffer.len() - (address as usize & (buffer.len() - 1));
        let size = (count - written).min(page_remaining as u64) as usize;
        output.fill_random(&mut buffer[..size]);
        if memory.write(address, &buffer[..size]).is_err() {
            // The bytes already copied stand; EFAULT only when none were.
            return if written == 0 { errno(EFAULT) } else { written };
        }
        written += size as u64;
        if (written / buffer.len() as u64).is_multiple_of(PAGES_PER_TURN) && written < count {
            yield_now().await;
        }
    }
    // The generator's state never outlives the call in the buffer.
    buffer.fill(0);
    written
}

pub(super) fn uname<M: UserMemory, O: System>(
    abi: Abi,
    args: [u64; 6],
    memory: &mut M,
    output: &O,
) -> u64 {
    // uapi/linux/utsname.h `struct new_utsname`: six fields of
    // `__NEW_UTS_LEN + 1` bytes, zero padded. Both ABIs' `uname` is
    // `newuname`. `UTS_MACHINE` is x86_64 and, on a little-endian arm64
    // kernel (arch/arm64/Makefile), aarch64.
    const FIELD: usize = 65;
    let machine = match abi {
        Abi::X86_64 => "x86_64",
        Abi::Aarch64 => "aarch64",
    };
    let fields = [
        "Linux",
        UNSET_NAME,
        RELEASE,
        output.kernel_version(),
        machine,
        UNSET_NAME,
    ];
    let mut bytes = [0; FIELD * 6];
    for (slot, value) in bytes.as_chunks_mut::<FIELD>().0.iter_mut().zip(fields) {
        // The last byte stays zero to terminate the string.
        let length = value.len().min(FIELD - 1);
        slot[..length].copy_from_slice(&value.as_bytes()[..length]);
    }
    write_result(memory, args[0], &bytes)
}

pub(super) fn sysinfo<M: UserMemory, O: Clock + System>(
    args: [u64; 6],
    memory: &mut M,
    output: &O,
) -> u64 {
    // uapi/linux/sysinfo.h `struct sysinfo` on LP64: 112 bytes.
    let mut bytes = [0; 112];
    let mut put = |offset: usize, value: u64| {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };
    // Seconds since boot, rounded up (kernel/sys.c `do_sysinfo`).
    put(0, output.now_nanos().div_ceil(NANOS_PER_SECOND));
    // `loads` (8..32) stay zero: Helios keeps no load average.
    let memory_totals = output.memory();
    put(32, memory_totals.total_bytes);
    put(40, memory_totals.free_bytes);
    // `sharedram`, `bufferram`, `totalswap`, `freeswap` and the high-memory
    // fields are zero here; `mem_unit` is 1 once `do_sysinfo` has scaled
    // the memory fields to bytes.
    bytes[80..82].copy_from_slice(
        &u16::try_from(output.process_count())
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    bytes[104..108].copy_from_slice(&1_u32.to_le_bytes());
    write_result(memory, args[0], &bytes)
}

impl Credentials {
    pub(super) fn id(&self, operation: Operation) -> u64 {
        u64::from(match operation {
            Operation::Getuid => self.user.real,
            Operation::Geteuid => self.user.effective,
            Operation::Getgid => self.group.real,
            Operation::Getegid => self.group.effective,
            _ => unreachable!("not an identity query"),
        })
    }

    /// kernel/sys.c `getresuid` and `getresgid`: three `put_user`s that
    /// stop at the first fault.
    pub(super) fn resolve<M: UserMemory>(
        &self,
        operation: Operation,
        args: [u64; 6],
        memory: &mut M,
    ) -> u64 {
        let ids = match operation {
            Operation::Getresuid => self.user,
            _ => self.group,
        };
        for (address, value) in args[..3].iter().zip([ids.real, ids.effective, ids.saved]) {
            if memory.write(*address, &value.to_le_bytes()).is_err() {
                return errno(EFAULT);
            }
        }
        0
    }

    /// kernel/groups.c `getgroups`.
    pub(super) fn getgroups<M: UserMemory>(&self, args: [u64; 6], memory: &mut M) -> u64 {
        let size = args[0] as u32 as i32;
        if size < 0 {
            return errno(EINVAL);
        }
        let count = self.supplementary_groups.len();
        if size != 0 {
            if count > size as usize {
                return errno(EINVAL);
            }
            let bytes: Vec<u8> = self
                .supplementary_groups
                .iter()
                .flat_map(|group| group.to_le_bytes())
                .collect();
            if !bytes.is_empty() && memory.write(args[1], &bytes).is_err() {
                return errno(EFAULT);
            }
        }
        count as u64
    }
}

impl<R: Resources> Process<R> {
    /// kernel/sys.c `prlimit64`, `getrlimit` and `setrlimit`.
    pub(super) fn rlimit<M: UserMemory>(
        &mut self,
        operation: Operation,
        args: [u64; 6],
        memory: &mut M,
    ) -> u64 {
        let read = |memory: &M, address: u64| -> Result<Rlimit, ()> {
            let mut bytes = [0; 16];
            memory.read(address, &mut bytes).map_err(|_| ())?;
            Ok(Rlimit {
                current: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                maximum: u64::from_le_bytes(bytes[8..].try_into().unwrap()),
            })
        };
        let (resource, new_pointer, old_pointer) = match operation {
            // prlimit64(pid, resource, new_rlim, old_rlim).
            Operation::Prlimit64 => (args[1] as u32, args[2], args[3]),
            Operation::Getrlimit => (args[0] as u32, 0, args[1]),
            _ => (args[0] as u32, args[1], 0),
        };
        let new = if new_pointer == 0 {
            None
        } else {
            match read(memory, new_pointer) {
                Ok(limit) => Some(limit),
                Err(()) => return errno(EFAULT),
            }
        };
        if operation == Operation::Prlimit64 && !self.names_self(args[0] as u32 as i32) {
            return errno(ESRCH);
        }
        let old = match self.set_limit(resource, new) {
            Ok(old) => old,
            Err(error) => return errno(error),
        };
        if old_pointer == 0 {
            return 0;
        }
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&old.current.to_le_bytes());
        bytes[8..].copy_from_slice(&old.maximum.to_le_bytes());
        // The new limit stands when this write fails, as it does on Linux.
        write_result(memory, old_pointer, &bytes)
    }

    /// kernel/sys.c `umask`: the previous mask, bits above the permission
    /// bits dropped from the new one.
    pub(super) fn umask(&mut self, args: [u64; 6]) -> u64 {
        let mut attributes = self.shared.attributes.lock();
        let old = attributes.umask;
        attributes.umask = args[0] as u32 & 0o777;
        u64::from(old)
    }

    /// Whether `who` names this process under `which`: a process is its own
    /// group's only member and its own user's only process that these calls
    /// can see.
    fn priority_target(&self, which: i32, who: i32) -> bool {
        // uapi/linux/resource.h PRIO_PROCESS, PRIO_PGRP and PRIO_USER.
        match which {
            0 => self.names_self(who),
            1 => who == 0,
            _ => who == 0 || who as u32 == self.shared.attributes.lock().credentials.user.real,
        }
    }

    /// kernel/sys.c `getpriority`: `20 - nice`.
    pub(super) fn getpriority(&self, args: [u64; 6]) -> u64 {
        let (which, who) = (args[0] as u32 as i32, args[1] as u32 as i32);
        if !(0..=2).contains(&which) {
            return errno(EINVAL);
        }
        if !self.priority_target(which, who) {
            return errno(ESRCH);
        }
        nice_to_rlimit(self.shared.attributes.lock().nice)
    }

    /// kernel/sys.c `setpriority`. Lowering the nice value passes the
    /// `RLIMIT_NICE` test of kernel/sched/syscalls.c `can_nice`; the
    /// `CAP_SYS_NICE` alternative has no counterpart in a process's
    /// authority, so it never applies.
    pub(super) fn setpriority(&mut self, args: [u64; 6]) -> u64 {
        let (which, who) = (args[0] as u32 as i32, args[1] as u32 as i32);
        if !(0..=2).contains(&which) {
            return errno(EINVAL);
        }
        // include/linux/sched/prio.h MIN_NICE and MAX_NICE.
        let nice = (args[2] as u32 as i32).clamp(-20, 19);
        if !self.priority_target(which, who) {
            return errno(ESRCH);
        }
        let mut attributes = self.shared.attributes.lock();
        let allowed = attributes.values[Resource::Nice as usize].current;
        if nice < attributes.nice && nice_to_rlimit(nice) > allowed {
            return errno(EACCES);
        }
        attributes.nice = nice;
        0
    }
}
