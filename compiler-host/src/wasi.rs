//! The imports the compiler plugin links against: exactly the set the
//! kernel defines for it (`add_wasi_p1_imports` and `add_wasi_thread_spawn`
//! in `kernel/src/wasmtime_adapter/component_host/service/preview1.rs`), so
//! the plugin instantiates here only if it instantiates there.
//!
//! The bodies follow WASI preview 1 on this system, as stock Wasmtime's
//! `wasmtime-wasi` gives it, where the kernel's are its own: `sched_yield`
//! yields the thread, and `clock_time_get` reads the realtime and monotonic
//! clocks by id and refuses the CPU-time clocks. Everything else matches the
//! kernel: no preopens, an environment of `RAYON_NUM_THREADS` alone, stdout
//! and stderr only (both to this process's stderr, because stdout carries the
//! request protocol), and `proc_exit` ends the call that made it.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use wasmtime::{Caller, Linker, Store};

use crate::{PluginState, PluginThread, THREAD_START_EXPORT, memory};

const PREVIEW1: &str = "wasi_snapshot_preview1";

// WASI preview 1 errno values (`wasi_snapshot_preview1.witx`).
const ERRNO_SUCCESS: i32 = 0;
const ERRNO_BADF: i32 = 8;
const ERRNO_FAULT: i32 = 21;
const ERRNO_INVAL: i32 = 28;

// WASI preview 1 clock ids.
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME: i32 = 2;
const CLOCK_THREAD_CPUTIME: i32 = 3;

/// `filetype::character_device`, what the kernel reports for stdio.
const FILETYPE_CHARACTER_DEVICE: u8 = 2;
/// `rights::fd_write | rights::poll_fd_readwrite`, the kernel's stdio rights.
const STDIO_RIGHTS: u64 = (1 << 6) | (1 << 27);

const RAYON_NUM_THREADS: &str = "RAYON_NUM_THREADS";

/// The largest thread id wasi-threads allows (`0x1fff_ffff`), the bound
/// the kernel's allocator stops at.
const MAX_THREAD_ID: i32 = 0x1fff_ffff;

/// A trap a host function raises.
#[derive(Debug, Error)]
enum HostTrap {
    #[error("the compiler plugin called proc_exit({code})")]
    ProcExit { code: i32 },
    #[error("failed to read the system's entropy source: {0}")]
    Entropy(#[source] getrandom::Error),
    #[error("thread-spawn ran before the plugin's runtime was built")]
    SpawnBeforeRuntime,
    #[error("thread {thread_id} was spawned after its runtime stopped joining threads")]
    ThreadQueueClosed { thread_id: i32 },
}

pub(crate) fn add_imports(linker: &mut Linker<PluginState>) -> wasmtime::Result<()> {
    linker.func_wrap(
        PREVIEW1,
        "random_get",
        |caller: Caller<'_, PluginState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let mut bytes = vec![0; len as u32 as usize];
            getrandom::fill(&mut bytes).map_err(HostTrap::Entropy)?;
            Ok(errno_of(memory::write(
                caller.data().memory(),
                ptr as u32,
                &bytes,
            )))
        },
    )?;
    linker.func_wrap(PREVIEW1, "sched_yield", || -> i32 {
        std::thread::yield_now();
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(
        PREVIEW1,
        "clock_time_get",
        |caller: Caller<'_, PluginState>, id: i32, _precision: i64, ptr: i32| -> i32 {
            let nanos = match id {
                CLOCK_REALTIME => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_nanos() as u64),
                CLOCK_MONOTONIC => caller.data().shared.clock_epoch.elapsed().as_nanos() as u64,
                CLOCK_PROCESS_CPUTIME | CLOCK_THREAD_CPUTIME => return ERRNO_BADF,
                _ => return ERRNO_INVAL,
            };
            errno_of(memory::write(
                caller.data().memory(),
                ptr as u32,
                &nanos.to_le_bytes(),
            ))
        },
    )?;
    linker.func_wrap(
        PREVIEW1,
        "fd_write",
        |caller: Caller<'_, PluginState>, fd: i32, iovs: i32, iovs_len: i32, nwritten: i32| {
            fd_write(
                caller.data(),
                fd,
                iovs as u32,
                iovs_len as u32,
                nwritten as u32,
            )
        },
    )?;
    linker.func_wrap(
        PREVIEW1,
        "path_open",
        |_fd: i32,
         _dirflags: i32,
         _path: i32,
         _path_len: i32,
         _oflags: i32,
         _fs_rights_base: i64,
         _fs_rights_inheriting: i64,
         _fdflags: i32,
         _opened_fd: i32|
         -> i32 { ERRNO_BADF },
    )?;
    linker.func_wrap(
        PREVIEW1,
        "environ_get",
        |caller: Caller<'_, PluginState>, environ: i32, buf: i32| -> i32 {
            let memory = caller.data().memory();
            let variable = rayon_variable(caller.data());
            let pointer = memory::write(memory, environ as u32, &(buf as u32).to_le_bytes());
            let bytes = memory::write(memory, buf as u32, variable.as_bytes());
            errno_of(pointer.and(bytes))
        },
    )?;
    linker.func_wrap(
        PREVIEW1,
        "environ_sizes_get",
        |caller: Caller<'_, PluginState>, count: i32, size: i32| -> i32 {
            let memory = caller.data().memory();
            let len = rayon_variable(caller.data()).len() as u32;
            let count = memory::write(memory, count as u32, &1_u32.to_le_bytes());
            let size = memory::write(memory, size as u32, &len.to_le_bytes());
            errno_of(count.and(size))
        },
    )?;
    linker.func_wrap(
        PREVIEW1,
        "fd_fdstat_get",
        |caller: Caller<'_, PluginState>, fd: i32, stat: i32| -> i32 {
            if !can_write(caller.data(), fd) {
                return ERRNO_BADF;
            }
            let mut bytes = [0_u8; 24];
            bytes[0] = FILETYPE_CHARACTER_DEVICE;
            bytes[8..16].copy_from_slice(&STDIO_RIGHTS.to_le_bytes());
            bytes[16..24].copy_from_slice(&STDIO_RIGHTS.to_le_bytes());
            errno_of(memory::write(caller.data().memory(), stat as u32, &bytes))
        },
    )?;
    linker.func_wrap(
        PREVIEW1,
        "fd_close",
        |mut caller: Caller<'_, PluginState>, fd: i32| -> i32 {
            let state = caller.data_mut();
            match fd {
                1 if state.stdout_open => state.stdout_open = false,
                2 if state.stderr_open => state.stderr_open = false,
                _ => return ERRNO_BADF,
            }
            ERRNO_SUCCESS
        },
    )?;
    linker.func_wrap(PREVIEW1, "fd_prestat_get", |_fd: i32, _buf: i32| -> i32 {
        ERRNO_BADF
    })?;
    linker.func_wrap(
        PREVIEW1,
        "fd_prestat_dir_name",
        |_fd: i32, _path: i32, _len: i32| -> i32 { ERRNO_BADF },
    )?;
    linker.func_wrap(PREVIEW1, "proc_exit", |code: i32| -> wasmtime::Result<()> {
        Err(HostTrap::ProcExit { code }.into())
    })?;
    linker.func_wrap("wasi", "thread-spawn", thread_spawn)?;
    Ok(())
}

/// `wasi.thread-spawn`: a native thread with its own store and instance
/// over the shared memory, running `wasi_thread_start(tid, start_arg)`.
fn thread_spawn(caller: Caller<'_, PluginState>, start_arg: i32) -> wasmtime::Result<i32> {
    let shared = caller.data().shared.clone();
    let instance_pre = shared
        .instance_pre
        .get()
        .ok_or(HostTrap::SpawnBeforeRuntime)?
        .clone();
    let Ok(previous) =
        shared
            .next_thread_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                (value < MAX_THREAD_ID).then_some(value + 1)
            })
    else {
        tracing::error!("the compiler plugin exhausted the wasi-threads thread ids");
        return Ok(-1);
    };
    let thread_id = previous + 1;
    let state = PluginState::new(shared.clone());
    let spawned = std::thread::Builder::new()
        .name(format!("compiler-plugin-{thread_id}"))
        .stack_size(crate::WASM_THREAD_STACK)
        .spawn(move || -> wasmtime::Result<()> {
            let mut store = Store::new(instance_pre.module().engine(), state);
            let instance = instance_pre.instantiate(&mut store)?;
            instance
                .get_typed_func::<(i32, i32), ()>(&mut store, THREAD_START_EXPORT)?
                .call(&mut store, (thread_id, start_arg))
        });
    match spawned {
        Ok(handle) => {
            // The receiver lives as long as the runtime that owns this
            // store's shared state, so the send cannot fail while a compile
            // runs.
            shared
                .threads
                .send(PluginThread { thread_id, handle })
                .map_err(|_| HostTrap::ThreadQueueClosed { thread_id })?;
            Ok(thread_id)
        }
        Err(error) => {
            tracing::error!(thread_id, %error, "failed to spawn a compiler plugin thread");
            Ok(-1)
        }
    }
}

fn fd_write(state: &PluginState, fd: i32, iovs: u32, iovs_len: u32, nwritten: u32) -> i32 {
    if !can_write(state, fd) {
        return ERRNO_BADF;
    }
    let memory = state.memory();
    let mut stderr = std::io::stderr().lock();
    let mut written = 0_u32;
    for index in 0..iovs_len {
        let iov = iovs.wrapping_add(index.wrapping_mul(8));
        let Ok(iov) = memory::read(memory, iov, 8) else {
            return ERRNO_FAULT;
        };
        let ptr = u32::from_le_bytes([iov[0], iov[1], iov[2], iov[3]]);
        let len = u32::from_le_bytes([iov[4], iov[5], iov[6], iov[7]]);
        let Ok(bytes) = memory::read(memory, ptr, len as usize) else {
            return ERRNO_FAULT;
        };
        if let Err(error) = stderr.write_all(&bytes) {
            tracing::error!(%error, fd, "failed to forward the compiler plugin's output");
            return ERRNO_BADF;
        }
        written = written.saturating_add(len);
    }
    errno_of(memory::write(memory, nwritten, &written.to_le_bytes()))
}

fn can_write(state: &PluginState, fd: i32) -> bool {
    match fd {
        1 => state.stdout_open,
        2 => state.stderr_open,
        _ => false,
    }
}

/// `RAYON_NUM_THREADS=<workers>` with its terminating NUL, byte for byte
/// what the kernel's `write_rayon_threads_env` writes.
fn rayon_variable(state: &PluginState) -> String {
    format!("{RAYON_NUM_THREADS}={}\0", state.shared.workers)
}

fn errno_of(result: Result<(), memory::GuestMemoryAccess>) -> i32 {
    result.map_or(ERRNO_FAULT, |()| ERRNO_SUCCESS)
}
