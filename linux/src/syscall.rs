use alloc::sync::Arc;
use bytes::Bytes;
use core::convert::Infallible;
use core::future::Future;
use core::num::NonZeroU32;
use core::ops::ControlFlow;
use core::sync::atomic::AtomicU64;
use core::task::{Context, Poll};
use helios_hal::vmm::PageFlags;
use helios_sync::{Notify, NotifyWaiter, RawMutex};
use helios_unix::{
    ChildTermination, DescriptorSlots, FileDescriptor, FileDescriptorEntry, FutexTable, NoNetwork,
    ProcessIdentity, ProcessMemoryIdentity, SocketNetwork,
};

mod futex;
mod io;
mod memory;
mod system;
mod thread;

pub use crate::abi::{Abi, Operation};
pub use io::Clock;
pub use system::{
    Credentials, Ids, MemoryTotals, NR_OPEN_DEFAULT, RELEASE, RLIM_INFINITY, Resource, Rlimit,
    Start, System,
};

/// The platform objects a process's descriptors can hold besides the UNIX
/// objects `helios-unix` owns (pipes, eventfds, epoll sets, sockets): the
/// host's files, the network its sockets live on, and the user data its
/// epoll interests carry. A process shares descriptors with programs of
/// another ABI by naming the same resource set they use.
pub trait Resources: 'static {
    type File: Clone + Send + Sync;
    type Network: SocketNetwork;
    type Data: EpollData + Send + Sync;
}

/// User data an epoll interest carries, as Linux's 64-bit `epoll_data`.
pub trait EpollData: Clone {
    fn from_linux(data: u64) -> Self;
    fn to_linux(&self) -> u64;
}

impl EpollData for u64 {
    fn from_linux(data: u64) -> Self {
        data
    }
    fn to_linux(&self) -> u64 {
        *self
    }
}

/// A resource set with no files and no network: its only sockets are
/// socket pairs.
pub struct NoBacking;
impl Resources for NoBacking {
    type File = Infallible;
    type Network = NoNetwork;
    type Data = u64;
}

pub type Descriptor<R> =
    FileDescriptor<<R as Resources>::File, <R as Resources>::Network, <R as Resources>::Data>;
pub type Entry<R> =
    FileDescriptorEntry<<R as Resources>::File, <R as Resources>::Network, <R as Resources>::Data>;
pub type Descriptors<R> = DescriptorSlots<Entry<R>>;

/// The descriptor table a process started by no other process holds:
/// standard input, output and error at 0, 1 and 2.
pub fn standard_streams<R: Resources>() -> Descriptors<R> {
    DescriptorSlots::from_entries(alloc::vec![
        Some(Entry::<R>::new(
            Descriptor::<R>::Stdin {
                carry: Bytes::new()
            },
            false
        )),
        Some(Entry::<R>::new(Descriptor::<R>::Stdout, false)),
        Some(Entry::<R>::new(Descriptor::<R>::Stderr, false)),
    ])
}

/// A fault reported by the address-space owner, before any bytes are exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("inaccessible user address")]
pub struct MemoryFault;

pub trait UserMemory {
    /// Copies exactly `output.len()` readable bytes. The implementation owns
    /// mapping synchronization; no user reference may escape this operation.
    fn read(&self, address: u64, output: &mut [u8]) -> Result<(), MemoryFault>;
    /// Validates a writable range without modifying it.
    fn check_writable(&self, address: u64, length: usize) -> Result<(), MemoryFault>;
    /// Copies bytes only into writable user mappings.
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryFault>;
    /// Atomically loads a 4-byte-aligned 32-bit word from readable user memory.
    fn atomic_load_u32(&self, address: u64) -> Result<u32, MemoryFault>;
    /// Atomically stores a 4-byte-aligned 32-bit word into writable user memory.
    fn atomic_store_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryFault>;
    /// Atomically compares and replaces a 4-byte-aligned 32-bit word in
    /// readable and writable user memory.
    fn atomic_compare_exchange_u32(
        &mut self,
        address: u64,
        expected: u32,
        desired: u32,
    ) -> Result<Result<u32, u32>, MemoryFault>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    #[error("user memory exhausted")]
    OutOfMemory,
    #[error("the range lies outside the mappable user range")]
    Address,
    #[error("the range is already mapped")]
    Occupied,
}

/// A run of pages mapped contiguously with one access, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub access: PageFlags,
}

/// The process's address space as its memory-map system calls change it.
/// Every range is page aligned, nonempty and below the ABI's `TASK_SIZE`;
/// the caller has validated it with Linux's errors. The process's shared
/// address-space lock serialises multi-step compositions across threads.
pub trait VirtualMemory: UserMemory {
    /// The lowest address the space maps.
    fn lowest_address(&self) -> u64;

    /// Installs zero-filled private pages at the first free range at or
    /// above `hint`, else anywhere, and returns where.
    fn map_anywhere(&mut self, hint: u64, length: u64, access: PageFlags) -> Result<u64, MapError>;

    /// Installs zero-filled private pages at `address`, which must have no
    /// mapping. Failure leaves the space unchanged.
    fn map_at(&mut self, address: u64, length: u64, access: PageFlags) -> Result<(), MapError>;

    /// Removes every mapping of the range; unmapped pages are skipped.
    fn unmap(&mut self, address: u64, length: u64);

    /// Replaces the access of the range, every page of which is mapped.
    fn protect(&mut self, address: u64, length: u64, access: PageFlags) -> Result<(), MapError>;

    /// Drops the contents of every mapped page of the range, which reads as
    /// zeros afterwards and holds no memory until it is touched.
    fn discard(&mut self, address: u64, length: u64);

    /// Moves `[from, from + length)`, one region, to `to`, which has no
    /// mapping, extending it with zero-filled pages of the same access to
    /// `new_length`. Failure leaves the space unchanged.
    fn remap(&mut self, from: u64, length: u64, to: u64, new_length: u64) -> Result<(), MapError>;

    /// The first region of `[start, end)`, clipped to it.
    fn region(&self, start: u64, end: u64) -> Option<Region>;

    /// The first free range of `length` bytes at or above `hint`, else
    /// anywhere.
    fn free_range(&self, hint: u64, length: u64) -> Option<u64>;
}

/// Initial stack and TLS overrides for a cloned thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnedThread {
    /// Initial user stack pointer (`Some(sp)`), or `None` to inherit the
    /// calling thread's stack pointer.
    pub stack: Option<u64>,
    /// Initial thread pointer when `CLONE_SETTLS` is set, or `None` to
    /// inherit the calling thread's thread pointer.
    pub tls: Option<u64>,
}

/// Why the execution host could not start a cloned thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    #[error("user or kernel memory exhausted")]
    OutOfMemory,
    #[error("thread task capacity exhausted")]
    Busy,
    #[error("invalid thread-pointer address")]
    InvalidTls,
}

/// The calling thread's execution context: its thread pointer (`arch_prctl`)
/// and spawning a sibling thread sharing this process (`clone` / `clone3`).
pub trait Thread<R: Resources = NoBacking> {
    fn thread_pointer(&self) -> u64;
    /// Refused for an address outside the user range.
    fn set_thread_pointer(&mut self, address: u64) -> Result<(), Denied>;
    /// Duplicates the calling thread's user context with `state` and starts
    /// `child` on its own task with syscall result `0`.
    fn spawn_thread(&mut self, child: Process<R>, state: SpawnedThread) -> Result<(), SpawnError>;
}

/// How an `exit` or `exit_group` system call ends execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `exit(2)`: ends the calling thread alone; ends the process with this
    /// status only when no sibling thread remains.
    Thread(u8),
    /// `exit_group(2)`: condemns every thread of the process with this status.
    Group(u8),
}

impl Exit {
    pub const fn status(self) -> u8 {
        match self {
            Self::Thread(status) | Self::Group(status) => status,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("output interrupted")]
    Interrupted,
    #[error("output failed")]
    Io,
    #[error("the process's authority does not reach this output")]
    Denied,
}

/// A refusal by the process's authority, or of an address it may not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("refused")]
pub struct Denied;

/// Where the standard-stream descriptors lead: the terminal, a parent's
/// channel, or a log, as the process's host routes them. The host refuses
/// a stream its process's authority does not reach.
pub trait Stdio {
    /// Writes up to the supplied length, including short writes. The buffer
    /// belongs to the caller and remains stable across suspension.
    fn write(
        &mut self,
        stream: Stream,
        bytes: &[u8],
    ) -> impl Future<Output = Result<usize, WriteError>> + Send;

    /// Waits for standard input and returns what arrived, at most `limit`
    /// bytes; empty at the end of input.
    fn read_input(&mut self, limit: usize) -> impl Future<Output = Result<Bytes, Denied>> + Send;
}

/// A system call as its ABI's registers carry it.
#[derive(Debug, Clone, Copy)]
pub struct Call {
    pub number: u64,
    pub arguments: [u64; 6],
}

// Linux uapi/asm-generic/errno-base.h and errno.h.
const EPERM: u64 = 1;
const EINTR: u64 = 4;
const EIO: u64 = 5;
const EBADF: u64 = 9;
const ENOMEM: u64 = 12;
const EACCES: u64 = 13;
const EFAULT: u64 = 14;
const ENODEV: u64 = 19;
const EINVAL: u64 = 22;
const ENOTTY: u64 = 25;
const ENOSYS: u64 = 38;
const EOPNOTSUPP: u64 = 95;
const MAX_RW_COUNT: u64 = 0x7fff_f000; // Linux INT_MAX rounded down to a page.

struct GroupLifecycle {
    threads: spin::Mutex<thread::ThreadGroup>,
    /// Broadcast whenever the group is condemned (`exit_group`, fatal signal,
    /// fault) or when the last live thread withdraws.
    changed: Notify,
}

/// Handle for observing and condemning a thread group's lifecycle without
/// retaining its descriptor table or other process resources.
#[derive(Clone)]
pub struct ProcessObserver {
    lifecycle: Arc<GroupLifecycle>,
}

impl ProcessObserver {
    pub fn group_ended(&self) -> bool {
        self.lifecycle.threads.lock().group_ended()
    }

    pub fn all_threads_exited(&self) -> bool {
        self.lifecycle.threads.lock().all_exited()
    }

    pub fn termination(&self) -> ChildTermination {
        self.lifecycle.threads.lock().termination()
    }

    pub fn waiter(&self) -> NotifyWaiter {
        self.lifecycle.changed.waiter()
    }

    pub fn poll_changed(&self, cx: &mut Context<'_>, waiter: &mut NotifyWaiter) -> Poll<()> {
        self.lifecycle.changed.poll_notified(cx, waiter)
    }

    pub fn terminate_group(&self, termination: ChildTermination) {
        let broadcast = self
            .lifecycle
            .threads
            .lock()
            .terminate_group(None, termination);
        if broadcast {
            self.lifecycle.changed.notify_all();
        }
    }
}

struct ProcessShared<R: Resources> {
    abi: Abi,
    identity: ProcessIdentity,
    memory_id: ProcessMemoryIdentity,
    futex: FutexTable,
    descriptors: spin::Mutex<Descriptors<R>>,
    stdin_lock: RawMutex,
    stdin_carry: spin::Mutex<Bytes>,
    attributes: spin::Mutex<system::Attributes>,
    program_break: spin::Mutex<memory::ProgramBreak>,
    threads: Arc<GroupLifecycle>,
}

impl core::ops::Deref for GroupLifecycle {
    type Target = spin::Mutex<thread::ThreadGroup>;
    fn deref(&self) -> &Self::Target {
        &self.threads
    }
}

/// One thread's handle to a Linux process (`CLONE_THREAD` shares the
/// underlying descriptor table, futex queues, program break and attributes).
///
/// # Concurrency
///
/// Each thread task exclusively owns its `Process<R>` handle while
/// dispatching a syscall. Shared process structures synchronize internally
/// with bounded non-awaiting critical sections (`descriptors`, `attributes`,
/// `program_break`, `threads`, `futex`) or an async mutex (`stdin_carry`);
/// no spin lock is ever held across an `.await`.
pub struct Process<R: Resources> {
    shared: Arc<ProcessShared<R>>,
    tid: NonZeroU32,
    clear_child_tid: u64,
    robust_list: Arc<AtomicU64>,
    exited: bool,
}

impl<R: Resources> Process<R> {
    /// A single-threaded process holding the standard streams only, whose
    /// program break starts at `program_break` (`Image::program_break`).
    pub fn new(abi: Abi, id: NonZeroU32, program_break: u64) -> Self {
        Self::with_descriptors(
            abi,
            id,
            program_break,
            standard_streams::<R>(),
            Start::default(),
        )
    }

    /// A process holding the descriptors its parent handed it, whose
    /// table's limit is its `RLIMIT_NOFILE`.
    pub fn with_descriptors(
        abi: Abi,
        id: NonZeroU32,
        program_break: u64,
        mut descriptors: Descriptors<R>,
        start: Start,
    ) -> Self {
        let open_files = descriptors.limit;
        let futex = start.futex.clone();
        let mut initial_stdin_carry = Bytes::new();
        for slot in descriptors.entries.iter_mut().flatten() {
            if let Descriptor::<R>::Stdin { carry } = &mut slot.descriptor
                && !carry.is_empty()
            {
                initial_stdin_carry = core::mem::take(carry);
                break;
            }
        }
        let robust_list = Arc::new(AtomicU64::new(0));
        let lifecycle = Arc::new(GroupLifecycle {
            threads: spin::Mutex::new(thread::ThreadGroup::new(id, robust_list.clone())),
            changed: Notify::new(),
        });
        Self {
            shared: Arc::new(ProcessShared {
                abi,
                identity: ProcessIdentity::new(id.into()),
                memory_id: ProcessMemoryIdentity::new(u64::from(id.get())),
                futex,
                descriptors: spin::Mutex::new(descriptors),
                stdin_lock: RawMutex::new(),
                stdin_carry: spin::Mutex::new(initial_stdin_carry),
                attributes: spin::Mutex::new(system::Attributes::new(start, open_files)),
                program_break: spin::Mutex::new(memory::ProgramBreak::new(program_break)),
                threads: lifecycle,
            }),
            tid: id,
            clear_child_tid: 0,
            robust_list,
            exited: false,
        }
    }

    /// The thread-group ID (`getpid`).
    pub fn pid(&self) -> NonZeroU32 {
        NonZeroU32::try_from(self.shared.identity.id())
            .expect("process identity is created from NonZeroU32")
    }

    /// This thread's ID (`gettid`).
    pub fn tid(&self) -> NonZeroU32 {
        self.tid
    }

    /// Observer handle for the thread group's lifecycle, independent of the
    /// descriptor table's reference count.
    pub fn observer(&self) -> ProcessObserver {
        ProcessObserver {
            lifecycle: self.shared.threads.clone(),
        }
    }

    /// Whether the thread group has been condemned (`exit_group`, fatal
    /// signal or fault).
    pub fn group_ended(&self) -> bool {
        self.shared.threads.lock().group_ended()
    }

    /// Arms a wait on thread-group condemnation.
    pub fn group_end_waiter(&self) -> NotifyWaiter {
        self.shared.threads.changed.waiter()
    }

    /// Polls a thread-group condemnation wait.
    pub fn poll_group_ended(&self, cx: &mut Context<'_>, waiter: &mut NotifyWaiter) -> Poll<()> {
        self.shared.threads.changed.poll_notified(cx, waiter)
    }

    /// Condemns the entire thread group from this thread (for a fatal signal,
    /// unhandled fault or owner kill) and withdraws this thread from the live
    /// set.
    pub fn terminate_group(&mut self, termination: ChildTermination) {
        let tid = (!self.exited).then_some(self.tid);
        self.exited = true;
        let broadcast = self.shared.threads.lock().terminate_group(tid, termination);
        if broadcast {
            self.shared.threads.changed.notify_all();
        }
    }

    fn finish_syscall_exit(&mut self, exit: Exit) {
        if self.exited {
            return;
        }
        self.exited = true;
        let (broadcast, all_exited) = self.shared.threads.lock().finish(self.tid, exit);
        if broadcast || all_exited {
            self.shared.threads.changed.notify_all();
        }
    }
}

impl<R: Resources> Drop for Process<R> {
    fn drop(&mut self) {
        if !self.exited {
            self.exited = true;
            let all_exited = self.shared.threads.lock().withdraw(self.tid);
            if all_exited {
                self.shared.threads.changed.notify_all();
            }
        }
    }
}

fn errno(value: u64) -> u64 {
    value.wrapping_neg()
}

/// Dispatches one Linux system call on `process`'s thread. `Exit` and
/// `ExitGroup` consume the thread's execution loop through `Break`.
pub async fn dispatch<R: Resources, M: VirtualMemory, T: Thread<R>, O: Stdio + Clock + System>(
    process: &mut Process<R>,
    call: Call,
    memory: &mut M,
    thread: &mut T,
    output: &mut O,
) -> ControlFlow<Exit, u64> {
    let abi = process.shared.abi;
    let arguments = call.arguments;
    let result = match abi.operation(call.number) {
        operation @ (Operation::Exit | Operation::ExitGroup) => {
            futex::exit_thread(process, memory);
            let status = arguments[0] as u8;
            let exit = if operation == Operation::ExitGroup {
                Exit::Group(status)
            } else {
                Exit::Thread(status)
            };
            process.finish_syscall_exit(exit);
            return ControlFlow::Break(exit);
        }
        Operation::Read => read::<R, _, _>(process, arguments, memory, output).await,
        Operation::Close => process
            .shared
            .descriptors
            .lock()
            .close(arguments[0] as u32 as i32)
            .map_or_else(io::descriptor_errno, |_| 0),
        operation @ (Operation::Pipe | Operation::Pipe2) => io::pipe::<R, _>(
            &mut process.shared.descriptors.lock(),
            arguments,
            memory,
            operation == Operation::Pipe2,
        ),
        operation @ (Operation::Dup | Operation::Dup2 | Operation::Dup3 | Operation::Fcntl) => {
            io::duplicate::<R>(&mut process.shared.descriptors.lock(), operation, arguments)
        }
        operation @ (Operation::Eventfd | Operation::Eventfd2) => io::event::<R>(
            &mut process.shared.descriptors.lock(),
            arguments,
            operation == Operation::Eventfd2,
        ),
        operation @ (Operation::EpollCreate | Operation::EpollCreate1) => io::epoll_create::<R>(
            &mut process.shared.descriptors.lock(),
            arguments,
            operation == Operation::EpollCreate1,
        ),
        Operation::EpollCtl => io::epoll_ctl::<R, _>(
            abi,
            &mut process.shared.descriptors.lock(),
            arguments,
            memory,
        ),
        operation @ (Operation::EpollWait | Operation::EpollPwait) => {
            io::epoll_wait::<R, _, _>(
                abi,
                &process.shared.descriptors,
                arguments,
                memory,
                output,
                operation == Operation::EpollPwait,
            )
            .await
        }
        Operation::Mmap => {
            let _mm = process.shared.program_break.lock();
            memory::mmap(abi, arguments, memory)
        }
        Operation::Munmap => {
            let _mm = process.shared.program_break.lock();
            memory::munmap(abi, arguments, memory)
        }
        Operation::Mprotect => {
            let _mm = process.shared.program_break.lock();
            memory::mprotect(abi, arguments, memory)
        }
        Operation::Mremap => {
            let _mm = process.shared.program_break.lock();
            memory::mremap(abi, arguments, memory)
        }
        Operation::Madvise => {
            let _mm = process.shared.program_break.lock();
            memory::madvise(arguments, memory)
        }
        Operation::Brk => process
            .shared
            .program_break
            .lock()
            .set(abi, arguments[0], memory),
        Operation::SchedYield => thread::sched_yield().await,
        Operation::SchedGetaffinity => {
            thread::sched_getaffinity(process, arguments, memory, output)
        }
        Operation::Nanosleep => thread::nanosleep(arguments, memory, output).await,
        Operation::ClockNanosleep => thread::clock_nanosleep(arguments, memory, output).await,
        Operation::Clone => thread::clone(process, arguments, memory, thread),
        Operation::Clone3 => thread::clone3(process, arguments, memory, thread),
        Operation::Futex => futex::futex(process, arguments, memory, output).await,
        Operation::SetRobustList => futex::set_robust_list(process, arguments),
        Operation::GetRobustList => futex::get_robust_list(process, arguments, memory),
        Operation::Getpid => process.shared.identity.id().get(),
        Operation::Gettid => u64::from(process.tid.get()),
        Operation::SetTidAddress => {
            process.clear_child_tid = arguments[0];
            u64::from(process.tid.get())
        }
        Operation::ArchPrctl => arch_prctl(abi, arguments, memory, thread),
        Operation::Writev => {
            writev::<R, _, _>(&process.shared.descriptors, arguments, memory, output).await
        }
        Operation::Ioctl => match process
            .shared
            .descriptors
            .lock()
            .get_entry(arguments[0] as u32 as i32)
        {
            Some(_) => errno(ENOTTY),
            None => errno(EBADF),
        },
        Operation::Write => {
            write::<R, _, _>(&process.shared.descriptors, arguments, memory, output).await
        }
        Operation::ClockGettime => system::clock_gettime(arguments, memory, output),
        Operation::ClockGetres => system::clock_getres(arguments, memory),
        Operation::ClockSettime => system::clock_settime(arguments, memory, output),
        Operation::Gettimeofday => system::gettimeofday(arguments, memory, output),
        Operation::Time => system::time(arguments, memory, output),
        Operation::Getrandom => system::getrandom(arguments, memory, output).await,
        Operation::Uname => system::uname(abi, arguments, memory, output),
        Operation::Sysinfo => system::sysinfo(arguments, memory, output),
        operation @ (Operation::Getuid
        | Operation::Geteuid
        | Operation::Getgid
        | Operation::Getegid) => process.shared.attributes.lock().credentials.id(operation),
        Operation::Getgroups => process
            .shared
            .attributes
            .lock()
            .credentials
            .getgroups(arguments, memory),
        Operation::Getresuid | Operation::Getresgid => {
            let credentials = process.shared.attributes.lock().credentials.clone();
            credentials.resolve(abi.operation(call.number), arguments, memory)
        }
        operation @ (Operation::Prlimit64 | Operation::Getrlimit | Operation::Setrlimit) => {
            process.rlimit(operation, arguments, memory)
        }
        Operation::Umask => process.umask(arguments),
        Operation::Getpriority => process.getpriority(arguments),
        Operation::Setpriority => process.setpriority(arguments),
        Operation::Unknown => errno(ENOSYS),
    };
    ControlFlow::Continue(result)
}

async fn read<R: Resources, M: UserMemory, O: Stdio>(
    process: &Process<R>,
    args: [u64; 6],
    memory: &mut M,
    output: &mut O,
) -> u64 {
    let Some(entry) = process
        .shared
        .descriptors
        .lock()
        .get_entry(args[0] as u32 as i32)
        .cloned()
    else {
        return errno(EBADF);
    };
    if matches!(entry.descriptor, Descriptor::<R>::Stdin { .. }) {
        let limit = args[2].min(MAX_RW_COUNT) as usize;
        if limit == 0 {
            return 0;
        }
        if memory.check_writable(args[1], limit).is_err() {
            return errno(EFAULT);
        }
        let stdin_guard = process.shared.stdin_lock.lock().await;
        if process.shared.stdin_carry.lock().is_empty() {
            match output.read_input(limit).await {
                Ok(bytes) => *process.shared.stdin_carry.lock() = bytes,
                Err(Denied) => return errno(EACCES),
            }
        }
        let bytes = {
            let mut carry = process.shared.stdin_carry.lock();
            let take = carry.len().min(limit);
            carry.split_to(take)
        };
        drop(stdin_guard);
        if memory.write(args[1], &bytes).is_err() {
            return errno(EFAULT);
        }
        return bytes.len() as u64;
    }
    io::read_resource::<R, M>(&entry, args, memory).await
}

async fn write<R: Resources, M: UserMemory, O: Stdio>(
    descriptors: &spin::Mutex<Descriptors<R>>,
    args: [u64; 6],
    memory: &M,
    output: &mut O,
) -> u64 {
    // Linux takes fd as an unsigned int, discarding the register's upper bits.
    let Some(entry) = descriptors.lock().get_entry(args[0] as u32 as i32).cloned() else {
        return errno(EBADF);
    };
    let stream = match &entry.descriptor {
        Descriptor::<R>::Stdout => Stream::Stdout,
        Descriptor::<R>::Stderr => Stream::Stderr,
        _ => return io::write_resource::<R, M>(&entry, args, memory).await,
    };
    // Linux MAX_RW_COUNT is INT_MAX rounded down to PAGE_SIZE (x86-64: 4096).
    let count = args[2].min(MAX_RW_COUNT);
    if args[1].checked_add(count).is_none() {
        return errno(EFAULT);
    }
    // One page bounds kernel stack use independently of the guest's count.
    let mut buffer = [0; 4096];
    let mut written = 0_u64;
    while written < count {
        let address = args[1] + written;
        // Stop at a user-page boundary so a fault on the next page preserves
        // the bytes already written, including an unaligned first buffer.
        let page_remaining = buffer.len() - (address as usize & (buffer.len() - 1));
        let size = (count - written).min(page_remaining as u64) as usize;
        if memory.read(address, &mut buffer[..size]).is_err() {
            return if written == 0 { errno(EFAULT) } else { written };
        }
        match output.write(stream, &buffer[..size]).await {
            Ok(n) => {
                assert!(n <= size, "output returned more bytes than supplied");
                written += n as u64;
                if n < size {
                    break;
                }
            }
            Err(error) => {
                return if written != 0 {
                    written
                } else {
                    errno(match error {
                        WriteError::Interrupted => EINTR,
                        WriteError::Io => EIO,
                        WriteError::Denied => EACCES,
                    })
                };
            }
        }
    }
    written
}

fn arch_prctl<R: Resources, M: UserMemory, T: Thread<R>>(
    abi: Abi,
    args: [u64; 6],
    memory: &mut M,
    thread: &mut T,
) -> u64 {
    // Linux UAPI asm/prctl.h: ARCH_SET_FS and ARCH_GET_FS.
    match args[0] as u32 {
        0x1002 => {
            // arch/x86/kernel/process_64.c refuses a base at or above
            // TASK_SIZE_MAX.
            if args[1] >= abi.task_size() || thread.set_thread_pointer(args[1]).is_err() {
                return errno(EPERM);
            }
            0
        }
        0x1003 => match memory.write(args[1], &thread.thread_pointer().to_le_bytes()) {
            Ok(()) => 0,
            Err(_) => errno(EFAULT),
        },
        _ => errno(EINVAL),
    }
}

async fn writev<R: Resources, M: UserMemory, O: Stdio>(
    descriptors: &spin::Mutex<Descriptors<R>>,
    args: [u64; 6],
    memory: &M,
    output: &mut O,
) -> u64 {
    if descriptors
        .lock()
        .get_entry(args[0] as u32 as i32)
        .is_none()
    {
        return errno(EBADF);
    }
    let count = args[2] as u32;
    // Linux UIO_MAXIOV; negative int values also fail this unsigned comparison.
    if count > 1024 {
        return errno(EINVAL);
    }
    if count == 0 {
        return 0;
    }
    if args[1].checked_add(u64::from(count) * 16).is_none() {
        return errno(EFAULT);
    }
    let mut vector = [0; 16];
    // Import/validate all lengths before emitting bytes, as Linux import_iovec
    // does.
    for index in 0..count {
        if memory
            .read(args[1] + u64::from(index) * 16, &mut vector)
            .is_err()
        {
            return errno(EFAULT);
        }
        let length = u64::from_le_bytes(vector[8..].try_into().unwrap());
        if length > i64::MAX as u64 {
            return errno(EINVAL);
        }
    }
    let mut done = 0;
    for index in 0..count {
        if done == MAX_RW_COUNT {
            break;
        }
        if memory
            .read(args[1] + u64::from(index) * 16, &mut vector)
            .is_err()
        {
            return if done == 0 { errno(EFAULT) } else { done };
        }
        let base = u64::from_le_bytes(vector[..8].try_into().unwrap());
        let length = u64::from_le_bytes(vector[8..].try_into().unwrap()).min(MAX_RW_COUNT - done);
        if length == 0 {
            continue;
        }
        let result = write::<R, _, _>(
            descriptors,
            [args[0], base, length, 0, 0, 0],
            memory,
            output,
        )
        .await;
        if (result as i64) < 0 {
            return if done == 0 { result } else { done };
        }
        done += result;
        if result < length {
            break;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_unix::OpenFile;

    // fcntl.h: O_CLOEXEC.
    const O_CLOEXEC: u64 = 0x80000;

    /// A node handle as the platform would supply one; only its identity
    /// matters here.
    #[derive(Clone)]
    struct Node;

    struct Files;
    impl Resources for Files {
        type File = Node;
        type Network = NoNetwork;
        type Data = u64;
    }

    fn open_file(descriptors: &Descriptors<Files>, fd: i32) -> &OpenFile<Node> {
        match descriptors.get_entry(fd).map(|entry| &entry.descriptor) {
            Some(Descriptor::<Files>::File(file)) => file,
            _ => panic!("descriptor {fd} must name the file"),
        }
    }

    #[test]
    fn duplicated_file_descriptors_share_one_open_file_description() {
        let process = Process::<Files>::new(Abi::X86_64, NonZeroU32::new(1).unwrap(), 0x40_0000);
        let mut descriptors = process.shared.descriptors.lock();
        let fd = descriptors
            .insert_entry(Entry::<Files>::new(
                Descriptor::<Files>::File(OpenFile::new(Node)),
                false,
            ))
            .expect("the table has room");
        let duplicate = io::duplicate::<Files>(
            &mut descriptors,
            Operation::Dup,
            [u64::from(fd), 0, 0, 0, 0, 0],
        );
        let target = io::duplicate::<Files>(
            &mut descriptors,
            Operation::Dup3,
            [u64::from(fd), 9, O_CLOEXEC, 0, 0, 0],
        );
        assert_eq!(target, 9);

        let original = open_file(&descriptors, fd as i32);
        original
            .try_lock_position()
            .expect("the position lock is free")
            .set(7);
        for copy in [duplicate as i32, 9] {
            let shared = open_file(&descriptors, copy);
            assert!(shared.same_description(original));
            assert_eq!(
                shared
                    .try_lock_position()
                    .expect("the position lock is free")
                    .get(),
                7
            );
        }
        // Only close-on-exec belongs to the slot.
        assert!(descriptors.get_entry(9).unwrap().close_on_exec);
        assert!(!descriptors.get_entry(fd as i32).unwrap().close_on_exec);
    }
}
