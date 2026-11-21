#![no_std]

//! Linux ABI policy for native execution. The platform owns execution and
//! memory; this crate never dereferences a guest address or selects a backend.
//! Each thread owns its [`syscall::Process`] handle while dispatching a
//! syscall, and threads of one process share their descriptor table, futex
//! queues, program break and process attributes without global state.

#[cfg(feature = "runtime")]
extern crate alloc;

#[cfg(feature = "runtime")]
pub mod abi;
#[cfg(feature = "runtime")]
pub mod elf;
#[cfg(feature = "runtime")]
pub mod syscall;

#[cfg(feature = "runtime")]
pub mod stack;
