#![cfg(feature = "runtime")]
//! Multi-threaded Linux system-call tests on both ABIs: `clone`, `clone3`,
//! `futex` (private and shared `WAIT`, `WAKE`, `REQUEUE`, `CMP_REQUEUE`,
//! `WAIT_BITSET`, `WAKE_BITSET`), `set_robust_list`/`get_robust_list` with
//! exit cleanup, `clear_child_tid` wake, `sched_yield`, `sched_getaffinity`,
//! `nanosleep`/`clock_nanosleep`, `getpid`/`gettid`, and `exit` vs
//! `exit_group`.

use core::future::Future;
use core::num::NonZeroU32;
use core::ops::ControlFlow;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};
use std::sync::{Arc, Mutex};

use helios_hal::vmm::PageFlags;
use helios_linux::abi::{Abi, Operation};
use helios_linux::syscall::{
    Call, Clock, Denied, Exit, MapError, MemoryFault, MemoryTotals, NoBacking, Process, Region,
    SpawnError, SpawnedThread, Start, Stdio, Stream, System, Thread, UserMemory, VirtualMemory,
    WriteError, dispatch,
};

const ABIS: [Abi; 2] = [Abi::X86_64, Abi::Aarch64];
const PROGRAM_BREAK: u64 = 0x40_0000;

const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SYSVSEM: u64 = 0x0004_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const CLONE_DETACHED: u64 = 0x0040_0000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

const THREAD_FLAGS: u64 =
    CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;

fn number(abi: Abi, operation: Operation) -> u64 {
    (0..512)
        .find(|candidate| abi.operation(*candidate) == operation)
        .unwrap_or_else(|| panic!("{abi:?} has no number for {operation:?}"))
}

#[derive(Clone)]
struct SharedMemory(Arc<Mutex<Vec<u8>>>);

impl SharedMemory {
    fn new(size: usize) -> Self {
        Self(Arc::new(Mutex::new(vec![0; size])))
    }

    fn u32_at(&self, address: u64) -> u32 {
        self.atomic_load_u32(address).unwrap()
    }

    fn set_u32(&mut self, address: u64, value: u32) {
        self.atomic_store_u32(address, value).unwrap();
    }

    fn set_u64(&mut self, address: u64, value: u64) {
        self.write(address, &value.to_le_bytes()).unwrap();
    }
}

impl UserMemory for SharedMemory {
    fn check_writable(&self, address: u64, length: usize) -> Result<(), MemoryFault> {
        let start = usize::try_from(address).map_err(|_| MemoryFault)?;
        let end = start.checked_add(length).ok_or(MemoryFault)?;
        let bytes = self.0.lock().unwrap();
        if start < 4096 || end > bytes.len() {
            return Err(MemoryFault);
        }
        Ok(())
    }

    fn read(&self, address: u64, output: &mut [u8]) -> Result<(), MemoryFault> {
        let start = usize::try_from(address).map_err(|_| MemoryFault)?;
        let end = start.checked_add(output.len()).ok_or(MemoryFault)?;
        let bytes = self.0.lock().unwrap();
        if start < 4096 || end > bytes.len() {
            return Err(MemoryFault);
        }
        output.copy_from_slice(&bytes[start..end]);
        Ok(())
    }

    fn write(&mut self, address: u64, input: &[u8]) -> Result<(), MemoryFault> {
        let start = usize::try_from(address).map_err(|_| MemoryFault)?;
        let end = start.checked_add(input.len()).ok_or(MemoryFault)?;
        let mut bytes = self.0.lock().unwrap();
        if start < 4096 || end > bytes.len() {
            return Err(MemoryFault);
        }
        bytes[start..end].copy_from_slice(input);
        Ok(())
    }

    fn atomic_load_u32(&self, address: u64) -> Result<u32, MemoryFault> {
        if address & 3 != 0 {
            return Err(MemoryFault);
        }
        let mut word = [0; 4];
        self.read(address, &mut word)?;
        Ok(u32::from_le_bytes(word))
    }

    fn atomic_store_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryFault> {
        if address & 3 != 0 {
            return Err(MemoryFault);
        }
        self.write(address, &value.to_le_bytes())
    }

    fn atomic_compare_exchange_u32(
        &mut self,
        address: u64,
        expected: u32,
        desired: u32,
    ) -> Result<Result<u32, u32>, MemoryFault> {
        if address & 3 != 0 {
            return Err(MemoryFault);
        }
        let start = usize::try_from(address).map_err(|_| MemoryFault)?;
        let end = start.checked_add(4).ok_or(MemoryFault)?;
        let mut bytes = self.0.lock().unwrap();
        if start < 4096 || end > bytes.len() {
            return Err(MemoryFault);
        }
        let current = u32::from_le_bytes(bytes[start..end].try_into().unwrap());
        if current == expected {
            bytes[start..end].copy_from_slice(&desired.to_le_bytes());
            Ok(Ok(current))
        } else {
            Ok(Err(current))
        }
    }
}

impl VirtualMemory for SharedMemory {
    fn lowest_address(&self) -> u64 {
        4096
    }
    fn map_anywhere(&mut self, _: u64, _: u64, _: PageFlags) -> Result<u64, MapError> {
        Err(MapError::OutOfMemory)
    }
    fn map_at(&mut self, _: u64, _: u64, _: PageFlags) -> Result<(), MapError> {
        Err(MapError::OutOfMemory)
    }
    fn unmap(&mut self, _: u64, _: u64) {}
    fn protect(&mut self, _: u64, _: u64, _: PageFlags) -> Result<(), MapError> {
        Err(MapError::OutOfMemory)
    }
    fn discard(&mut self, _: u64, _: u64) {}
    fn remap(&mut self, _: u64, _: u64, _: u64, _: u64) -> Result<(), MapError> {
        Err(MapError::OutOfMemory)
    }
    fn region(&self, _: u64, _: u64) -> Option<Region> {
        None
    }
    fn free_range(&self, _: u64, _: u64) -> Option<u64> {
        None
    }
}

struct HostThread {
    tls: u64,
    spawned: Vec<(Process<NoBacking>, SpawnedThread)>,
}

impl HostThread {
    fn new() -> Self {
        Self {
            tls: 0,
            spawned: Vec::new(),
        }
    }
}

impl Thread for HostThread {
    fn thread_pointer(&self) -> u64 {
        self.tls
    }
    fn set_thread_pointer(&mut self, address: u64) -> Result<(), Denied> {
        self.tls = address;
        Ok(())
    }
    fn spawn_thread(
        &mut self,
        child: Process<NoBacking>,
        state: SpawnedThread,
    ) -> Result<(), SpawnError> {
        self.spawned.push((child, state));
        Ok(())
    }
}

struct Platform {
    monotonic: AtomicU64,
    realtime: u64,
    processors: u32,
    sleep_pending: bool,
}

impl Default for Platform {
    fn default() -> Self {
        Self {
            monotonic: AtomicU64::new(1_000_000_000),
            realtime: 1_700_000_000_000_000_000,
            processors: 4,
            sleep_pending: false,
        }
    }
}

impl Stdio for Platform {
    async fn write(&mut self, _: Stream, bytes: &[u8]) -> Result<usize, WriteError> {
        Ok(bytes.len())
    }
    async fn read_input(&mut self, _: usize) -> Result<bytes::Bytes, Denied> {
        Ok(bytes::Bytes::new())
    }
}

impl Clock for Platform {
    fn now_nanos(&self) -> u64 {
        self.monotonic.load(Ordering::Relaxed)
    }
    async fn sleep_until(&self, deadline: u64) {
        if self.sleep_pending {
            core::future::pending::<()>().await;
        } else {
            self.monotonic.store(deadline, Ordering::Relaxed);
        }
    }
}

impl System for Platform {
    fn realtime_nanos(&self) -> u64 {
        self.realtime
    }
    fn set_realtime_nanos(&mut self, nanos: u64) -> Result<(), Denied> {
        self.realtime = nanos;
        Ok(())
    }
    fn cpu_nanos(&self) -> u64 {
        5_000
    }
    fn process_cpu_nanos(&self) -> u64 {
        15_000
    }
    fn processor_count(&self) -> u32 {
        self.processors
    }
    fn fill_random(&mut self, output: &mut [u8]) {
        output.fill(0x5a);
    }
    fn memory(&self) -> MemoryTotals {
        MemoryTotals {
            total_bytes: 1 << 30,
            free_bytes: 1 << 29,
        }
    }
    fn process_count(&self) -> u32 {
        1
    }
    fn kernel_version(&self) -> &str {
        "#1 Helios test"
    }
}

fn drive<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            return output;
        }
    }
}

fn poll_once<T>(mut future: Pin<&mut impl Future<Output = T>>, waker: &Waker) -> Poll<T> {
    future.as_mut().poll(&mut Context::from_waker(waker))
}

struct CountingWake(Arc<AtomicU32>);
impl std::task::Wake for CountingWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn clone_and_clone3_assign_distinct_tids_share_tgid_and_handle_exit_vs_exit_group() {
    for abi in ABIS {
        let mut memory = SharedMemory::new(0x10000);
        let mut platform = Platform::default();
        let mut main_thread = HostThread::new();
        let mut main = Process::<NoBacking>::with_descriptors(
            abi,
            NonZeroU32::new(100).unwrap(),
            PROGRAM_BREAK,
            helios_linux::syscall::standard_streams::<NoBacking>(),
            Start {
                memory_bytes: 64 << 20,
                ..Start::default()
            },
        );
        let observer = main.observer();

        let ptid_slot = 0x2000;
        let ctid_slot = 0x2008;
        let flags = THREAD_FLAGS
            | CLONE_SETTLS
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_CLEARTID
            | CLONE_DETACHED;
        let clone_args = match abi {
            Abi::X86_64 => [flags, 0x8000, ptid_slot, ctid_slot, 0x9000, 0],
            Abi::Aarch64 => [flags, 0x8000, ptid_slot, 0x9000, ctid_slot, 0],
        };
        let child1_tid = match drive(dispatch(
            &mut main,
            Call {
                number: number(abi, Operation::Clone),
                arguments: clone_args,
            },
            &mut memory,
            &mut main_thread,
            &mut platform,
        )) {
            ControlFlow::Continue(tid) => tid,
            ControlFlow::Break(_) => panic!("clone ended caller"),
        };
        assert_eq!(child1_tid, 101);
        assert_eq!(memory.u32_at(ptid_slot), 101);
        let (mut child1, state1) = main_thread.spawned.pop().unwrap();
        assert_eq!(
            state1,
            SpawnedThread {
                stack: Some(0x8000),
                tls: Some(0x9000),
            }
        );

        // `clone3` lays stack out as `[base, base + size)` and rejects `CLONE_DETACHED`.
        let clone3_struct = 0x3000;
        memory.set_u64(
            clone3_struct,
            THREAD_FLAGS | CLONE_PARENT_SETTID | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID,
        );
        memory.set_u64(clone3_struct + 16, 0x2010); // child_tid
        memory.set_u64(clone3_struct + 24, 0x2018); // parent_tid
        memory.set_u64(clone3_struct + 40, 0x4000); // stack base
        memory.set_u64(clone3_struct + 48, 0x2000); // stack size -> sp = 0x6000
        let child2_tid = match drive(dispatch(
            &mut main,
            Call {
                number: number(abi, Operation::Clone3),
                arguments: [clone3_struct, 88, 0, 0, 0, 0],
            },
            &mut memory,
            &mut main_thread,
            &mut platform,
        )) {
            ControlFlow::Continue(tid) => tid,
            ControlFlow::Break(_) => panic!("clone3 ended caller"),
        };
        assert_eq!(child2_tid, 102);
        assert_eq!(memory.u32_at(0x2010), 102);
        assert_eq!(memory.u32_at(0x2018), 102);
        let (mut child2, state2) = main_thread.spawned.pop().unwrap();
        assert_eq!(state2.stack, Some(0x6000));

        // `getpid` returns tgid (100) on all threads; `gettid` returns each thread's tid.
        for (proc_ref, expected_tid) in
            [(&mut main, 100u64), (&mut child1, 101), (&mut child2, 102)]
        {
            let mut th = HostThread::new();
            assert_eq!(
                drive(dispatch(
                    proc_ref,
                    Call {
                        number: number(abi, Operation::Getpid),
                        arguments: [0; 6]
                    },
                    &mut memory,
                    &mut th,
                    &mut platform,
                )),
                ControlFlow::Continue(100)
            );
            assert_eq!(
                drive(dispatch(
                    proc_ref,
                    Call {
                        number: number(abi, Operation::Gettid),
                        arguments: [0; 6]
                    },
                    &mut memory,
                    &mut th,
                    &mut platform,
                )),
                ControlFlow::Continue(expected_tid)
            );
        }

        // Single-thread `exit` on `child1` clears `ctid_slot` and wakes any
        // shared waiter on `ctid_slot`, without ending the thread group.
        memory.set_u32(ctid_slot, 101);
        let wakes = Arc::new(AtomicU32::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut wait_mem = memory.clone();
        let mut wait_platform = Platform {
            sleep_pending: true,
            ..Platform::default()
        };
        let mut wait_thread = HostThread::new();
        {
            let mut wait_fut = std::pin::pin!(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::Futex),
                    arguments: [ctid_slot, FUTEX_WAIT, 101, 0, 0, 0],
                },
                &mut wait_mem,
                &mut wait_thread,
                &mut wait_platform,
            ));
            assert!(poll_once(wait_fut.as_mut(), &waker).is_pending());

            let mut c1_thread = HostThread::new();
            assert_eq!(
                drive(dispatch(
                    &mut child1,
                    Call {
                        number: number(abi, Operation::Exit),
                        arguments: [7, 0, 0, 0, 0, 0],
                    },
                    &mut memory,
                    &mut c1_thread,
                    &mut platform,
                )),
                ControlFlow::Break(Exit::Thread(7))
            );
            assert!(!observer.group_ended());
            assert!(!observer.all_threads_exited());
            assert_eq!(memory.u32_at(ctid_slot), 0);
            assert!(wakes.load(Ordering::SeqCst) > 0);
            assert_eq!(
                poll_once(wait_fut.as_mut(), &waker),
                Poll::Ready(ControlFlow::Continue(0))
            );
        }

        // `exit_group` from non-main `child2` condemns the group with its status.
        let mut c2_thread = HostThread::new();
        assert_eq!(
            drive(dispatch(
                &mut child2,
                Call {
                    number: number(abi, Operation::ExitGroup),
                    arguments: [42, 0, 0, 0, 0, 0],
                },
                &mut memory,
                &mut c2_thread,
                &mut platform,
            )),
            ControlFlow::Break(Exit::Group(42))
        );
        assert!(observer.group_ended());
        assert_eq!(
            observer.termination(),
            helios_unix::ChildTermination::Exited(42)
        );
        drop(main);
        drop(child1);
        drop(child2);
        assert!(observer.all_threads_exited());
    }
}

#[test]
fn futex_operations_and_robust_list_cleanup_match_linux_semantics() {
    for abi in ABIS {
        let mut memory = SharedMemory::new(0x10000);
        let mut platform = Platform::default();
        let mut main_thread = HostThread::new();
        let mut main = Process::<NoBacking>::with_descriptors(
            abi,
            NonZeroU32::new(10).unwrap(),
            PROGRAM_BREAK,
            helios_linux::syscall::standard_streams::<NoBacking>(),
            Start {
                memory_bytes: 64 << 20,
                ..Start::default()
            },
        );
        for _ in 0..3 {
            let clone_args = match abi {
                Abi::X86_64 => [THREAD_FLAGS, 0x8000, 0, 0, 0, 0],
                Abi::Aarch64 => [THREAD_FLAGS, 0x8000, 0, 0, 0, 0],
            };
            let _ = drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::Clone),
                    arguments: clone_args,
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            ));
        }
        let (mut t1, _) = main_thread.spawned.remove(0);
        let (mut t2, _) = main_thread.spawned.remove(0);
        let (mut t3, _) = main_thread.spawned.remove(0);

        // Private vs shared queues are isolated at the same address.
        let word1 = 0x2000;
        let word2 = 0x2004;
        memory.set_u32(word1, 1);
        memory.set_u32(word2, 0);

        let waker = Waker::noop();
        let mut m1 = memory.clone();
        let mut m2 = memory.clone();
        let mut p1 = Platform::default();
        let mut p2 = Platform::default();
        let mut th1 = HostThread::new();
        let mut th2 = HostThread::new();
        {
            let mut f1 = std::pin::pin!(dispatch(
                &mut t1,
                Call {
                    number: number(abi, Operation::Futex),
                    arguments: [word1, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0],
                },
                &mut m1,
                &mut th1,
                &mut p1,
            ));
            let mut f2 = std::pin::pin!(dispatch(
                &mut t2,
                Call {
                    number: number(abi, Operation::Futex),
                    arguments: [word1, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0],
                },
                &mut m2,
                &mut th2,
                &mut p2,
            ));
            assert!(poll_once(f1.as_mut(), waker).is_pending());
            assert!(poll_once(f2.as_mut(), waker).is_pending());

            // Shared wake does not wake private waiters.
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [word1, FUTEX_WAKE, 2, 0, 0, 0],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(0)
            );

            // CMP_REQUEUE returns -EAGAIN on mismatch, and `woken + requeued` on match.
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [
                            word1,
                            FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                            1,
                            1,
                            word2,
                            99
                        ],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(11u64.wrapping_neg())
            );
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [
                            word1,
                            FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                            1,
                            1,
                            word2,
                            1
                        ],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(2)
            );
            assert_eq!(
                poll_once(f1.as_mut(), waker),
                Poll::Ready(ControlFlow::Continue(0))
            );
            assert!(poll_once(f2.as_mut(), waker).is_pending());

            // Plain `FUTEX_REQUEUE` shares `futex_requeue` with
            // `FUTEX_CMP_REQUEUE` and returns `woken + requeued`: requeue `t2`
            // back from `word2` to `word1` with 0 woken -> returns 1, then wake
            // `t2` on `word1`.
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [word2, FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG, 0, 1, word1, 0],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(1)
            );
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [word1, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(1)
            );
            assert_eq!(
                poll_once(f2.as_mut(), waker),
                Poll::Ready(ControlFlow::Continue(0))
            );
        }

        // WAIT_BITSET / WAKE_BITSET selective bitmask wakeup and absolute timeout.
        {
            let mut f1 = std::pin::pin!(dispatch(
                &mut t1,
                Call {
                    number: number(abi, Operation::Futex),
                    arguments: [
                        word1,
                        FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
                        1,
                        0,
                        0,
                        0b0010
                    ],
                },
                &mut m1,
                &mut th1,
                &mut p1,
            ));
            assert!(poll_once(f1.as_mut(), waker).is_pending());
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [
                            word1,
                            FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG,
                            1,
                            0,
                            0,
                            0b0001
                        ],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(0)
            );
            assert_eq!(
                drive(dispatch(
                    &mut main,
                    Call {
                        number: number(abi, Operation::Futex),
                        arguments: [
                            word1,
                            FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG,
                            1,
                            0,
                            0,
                            0b0010
                        ],
                    },
                    &mut memory,
                    &mut main_thread,
                    &mut platform,
                )),
                ControlFlow::Continue(1)
            );
            assert_eq!(
                poll_once(f1.as_mut(), waker),
                Poll::Ready(ControlFlow::Continue(0))
            );
        }
        // Expired absolute realtime timeout returns -ETIMEDOUT (110).
        memory.set_u64(0x2100, 1);
        memory.set_u64(0x2108, 0);
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::Futex),
                    arguments: [
                        word1,
                        FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME,
                        1,
                        0x2100,
                        0,
                        u32::MAX as u64,
                    ],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue(110u64.wrapping_neg())
        );

        // Robust list registration, cross-thread query, and owner-died wake on `t3` exit.
        let head = 0x3000u64;
        let node = 0x3100u64;
        let lock_word = 0x3110u64; // futex_offset = +16
        memory.set_u64(head, node); // list.next -> node
        memory.set_u64(head + 8, 16); // futex_offset
        memory.set_u64(head + 16, 0); // list_op_pending
        memory.set_u64(node, head); // node.next -> head
        let t3_tid = t3.tid().get();
        memory.set_u32(lock_word, t3_tid | FUTEX_WAITERS);

        let mut th3 = HostThread::new();
        assert_eq!(
            drive(dispatch(
                &mut t3,
                Call {
                    number: number(abi, Operation::SetRobustList),
                    arguments: [head, 24, 0, 0, 0, 0],
                },
                &mut memory,
                &mut th3,
                &mut platform,
            )),
            ControlFlow::Continue(0)
        );
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::GetRobustList),
                    arguments: [u64::from(t3_tid), 0x3200, 0x3208, 0, 0, 0],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue(0)
        );
        let mut out_head = [0u8; 8];
        let mut out_len = [0u8; 8];
        memory.read(0x3200, &mut out_head).unwrap();
        memory.read(0x3208, &mut out_len).unwrap();
        assert_eq!(u64::from_le_bytes(out_head), head);
        assert_eq!(u64::from_le_bytes(out_len), 24);

        {
            let mut waiter = std::pin::pin!(dispatch(
                &mut t1,
                Call {
                    number: number(abi, Operation::Futex),
                    arguments: [
                        lock_word,
                        FUTEX_WAIT,
                        t3_tid as u64 | u64::from(FUTEX_WAITERS),
                        0,
                        0,
                        0
                    ],
                },
                &mut m1,
                &mut th1,
                &mut p1,
            ));
            assert!(poll_once(waiter.as_mut(), waker).is_pending());
            assert_eq!(
                drive(dispatch(
                    &mut t3,
                    Call {
                        number: number(abi, Operation::Exit),
                        arguments: [0; 6],
                    },
                    &mut memory,
                    &mut th3,
                    &mut platform,
                )),
                ControlFlow::Break(Exit::Thread(0))
            );
            assert_eq!(memory.u32_at(lock_word), FUTEX_WAITERS | FUTEX_OWNER_DIED);
            assert_eq!(
                poll_once(waiter.as_mut(), waker),
                Poll::Ready(ControlFlow::Continue(0))
            );
        }

        // `sched_yield`, `sched_getaffinity`, `nanosleep`, and `clock_nanosleep`.
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::SchedYield),
                    arguments: [0; 6],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue(0)
        );
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::SchedGetaffinity),
                    arguments: [0, 16, 0x3300, 0, 0, 0],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue(8)
        );
        assert_eq!(memory.u32_at(0x3300), 0b1111);
        // Negative PIDs answer ESRCH on Linux, while invalid mask length is
        // checked first and answers EINVAL.
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::SchedGetaffinity),
                    arguments: [u64::from(u32::MAX), 16, 0x3300, 0, 0, 0],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue((-3i64) as u64)
        );
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::SchedGetaffinity),
                    arguments: [u64::from(u32::MAX), 4, 0x3300, 0, 0, 0],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue((-22i64) as u64)
        );
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::GetRobustList),
                    arguments: [u64::from(u32::MAX), 0x3200, 0x3208, 0, 0, 0],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue((-3i64) as u64)
        );

        memory.set_u64(0x3400, 0);
        memory.set_u64(0x3408, 25_000_000);
        let before = platform.now_nanos();
        assert_eq!(
            drive(dispatch(
                &mut main,
                Call {
                    number: number(abi, Operation::Nanosleep),
                    arguments: [0x3400, 0, 0, 0, 0, 0],
                },
                &mut memory,
                &mut main_thread,
                &mut platform,
            )),
            ControlFlow::Continue(0)
        );
        assert_eq!(platform.now_nanos(), before + 25_000_000);
    }
}

#[test]
fn dispatch_future_is_send() {
    fn require_send<F: Future + Send>(future: F) -> F {
        future
    }

    let mut process =
        Process::<NoBacking>::new(Abi::X86_64, NonZeroU32::new(1).unwrap(), PROGRAM_BREAK);
    let mut memory = SharedMemory::new(0x10_000);
    let mut thread = HostThread::new();
    let mut platform = Platform::default();
    let future = require_send(dispatch(
        &mut process,
        Call {
            number: number(Abi::X86_64, Operation::Gettid),
            arguments: [0; 6],
        },
        &mut memory,
        &mut thread,
        &mut platform,
    ));
    assert_eq!(drive(future), ControlFlow::Continue(1));
}

#[test]
fn inherited_stdin_carry_is_delivered_before_requesting_more_input() {
    let slots =
        helios_unix::DescriptorSlots::from_entries(vec![Some(helios_linux::syscall::Entry::<
            NoBacking,
        >::new(
            helios_linux::syscall::Descriptor::<NoBacking>::Stdin {
                carry: bytes::Bytes::from_static("中文 日本語 한국어".as_bytes()),
            },
            false,
        ))]);
    let mut process = Process::<NoBacking>::with_descriptors(
        Abi::X86_64,
        NonZeroU32::new(1).unwrap(),
        PROGRAM_BREAK,
        slots,
        Start::default(),
    );
    let mut memory = SharedMemory::new(0x10_000);
    let mut thread = HostThread::new();
    let mut platform = Platform::default();

    assert_eq!(
        drive(dispatch(
            &mut process,
            Call {
                number: number(Abi::X86_64, Operation::Read),
                arguments: [0, 0x2000, 64, 0, 0, 0],
            },
            &mut memory,
            &mut thread,
            &mut platform,
        )),
        ControlFlow::Continue("中文 日本語 한국어".len() as u64)
    );
    let mut out = vec![0u8; "中文 日本語 한국어".len()];
    memory.read(0x2000, &mut out).unwrap();
    assert_eq!(out, "中文 日本語 한국어".as_bytes());
}
