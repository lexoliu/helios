//! `oob-load`: reads past the end of its own linear memory on purpose.
//!
//! The kernel reserves 4 GiB of virtual address space behind every wasm
//! linear memory and commits only the pages a guest has actually grown
//! into. A wasm32 guest cannot form an address outside that reservation,
//! which is exactly why Cranelift emits no bounds check for its loads and
//! stores: an access past the committed pages lands on reserved memory and
//! the hardware raises the fault instead.
//!
//! That makes the fault path load-bearing rather than exceptional, and this
//! program is what exercises it end to end. The read below must arrive at
//! the runtime's trap handler and come back out as a wasm trap that kills
//! this instance alone — not as an unhandled kernel page fault, and not as
//! a successful read of somebody else's memory. The marker printed first is
//! how a caller tells "trapped where expected" from "never got that far";
//! whatever the caller runs next is how it tells a trapped guest from a
//! dead machine.

use std::hint::black_box;
use std::ptr;

use thiserror::Error;

/// Byte offset to read. It is past any linear memory this program will be
/// given and inside the reservation the runtime puts behind it, so the
/// access is one the compiler leaves uninstrumented.
const OUT_OF_BOUNDS_OFFSET: usize = 0xFFFF_F000;

/// Printed before the faulting read, so a caller can tell a guest that
/// trapped at the read from one that failed to start.
const BEFORE_FAULT_MARKER: &str = "oob-load:before-fault";

#[derive(Debug, Error)]
enum OobLoadError {
    #[error("usage: oob-load")]
    UnexpectedArgument(String),
    #[error(
        "reading offset {OUT_OF_BOUNDS_OFFSET:#x} returned {0:#x} instead of trapping: this \
         instance can read memory outside its own linear memory"
    )]
    ReadSucceeded(u8),
}

#[helios_api::main]
async fn main() -> Result<(), OobLoadError> {
    if let Some(argument) = std::env::args().nth(1) {
        return Err(OobLoadError::UnexpectedArgument(argument));
    }

    println!("{BEFORE_FAULT_MARKER}");

    // `black_box` keeps the address opaque so the compiler cannot fold the
    // access into an unconditional trap at compile time, and `read_volatile`
    // keeps the load itself from being dropped as unused. Reaching the line
    // after this one means the access did not fault, which is a failure of
    // the reservation rather than of the guest.
    let address = black_box(OUT_OF_BOUNDS_OFFSET) as *const u8;
    // SAFETY: nothing about this read is sound — that is the point. The
    // address is deliberately outside every object this program owns, and
    // the runtime is expected to trap the instance before the value is
    // observed.
    let value = unsafe { ptr::read_volatile(address) };

    Err(OobLoadError::ReadSucceeded(black_box(value)))
}
