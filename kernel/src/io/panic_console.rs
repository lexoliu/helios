//! The panic console.
//!
//! The last thing a machine says is its panic report, and every backend
//! says it the same way: wrap a byte writer in a `fmt::Write` and push
//! the report through it. What differs is only which port a panicking
//! machine can still reach.

use core::fmt::{self, Write};
use core::marker::PhantomData;
use core::panic::PanicInfo;

/// The byte sink a panic report reaches on this machine.
///
/// A panic runs with nothing left to rely on: the executor may be gone,
/// the heap may be what broke, and the processors that share this port
/// are not going to stop for it. So a backend supplies a register-level
/// writer and nothing more — no allocation, no async lock, and at most
/// the sub-microsecond spin on a transmit FIFO that the async-first
/// rules allow.
///
/// The port a backend names here need not be the one its debugger
/// transport uses. riscv panics over SBI rather than over the transport
/// it hands the debugger, because SBI answers from firmware whatever
/// state the kernel left the machine in.
///
/// An implementation drops the bytes when the port is not up yet: a
/// panic raised inside the panic handler loses the report entirely.
pub trait PanicSerial {
    fn write_bytes(bytes: &[u8]);
}

struct PanicConsole<Serial>(PhantomData<Serial>);

impl<Serial: PanicSerial> Write for PanicConsole<Serial> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        Serial::write_bytes(text.as_bytes());
        Ok(())
    }
}

/// Writes one panic report to the machine's panic console.
///
/// It is the one record on this machine that does not go through
/// [`DebugConsole`](super::DebugConsole), for the reason that module
/// gives: a panicking processor cannot wait for a port another
/// processor may never release, and it cannot wait for a gate either.
/// The report goes straight at the register and accepts that it may cut
/// into whatever was on the wire.
///
/// The record names itself twice over, and both names are load-bearing:
/// `Kernel panic` is what a smoke run greps for to prove the kernel did
/// not panic, and the `panicked at …` that `PanicInfo` renders is what
/// the inspector's readiness reader watches for to stop waiting on a
/// guest that will never come up.
pub fn emit_panic_report<Serial: PanicSerial>(info: &PanicInfo<'_>) {
    emit_report::<Serial>(info);
}

/// The body of the report, over anything that renders like one.
///
/// A `PanicInfo` cannot be built outside a real panic, so this is where
/// the record's shape is testable.
fn emit_report<Serial: PanicSerial>(report: impl fmt::Display) {
    let mut console = PanicConsole::<Serial>(PhantomData);
    let _ = writeln!(console, "Kernel panic: {report}");
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec::Vec;
    use std::sync::Mutex;

    use super::{PanicSerial, emit_report};

    static CAPTURED: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    struct CapturingPort;

    impl PanicSerial for CapturingPort {
        fn write_bytes(bytes: &[u8]) {
            CAPTURED
                .lock()
                .expect("the capture buffer is never poisoned")
                .extend_from_slice(bytes);
        }
    }

    #[test]
    fn a_report_carries_both_names_the_tooling_watches_for() {
        emit_report::<CapturingPort>(format_args!(
            "panicked at kernel/src/lib.rs:1:1:\nthe machine gave up"
        ));

        let captured = CAPTURED
            .lock()
            .expect("the capture buffer is never poisoned")
            .clone();
        let text = core::str::from_utf8(&captured).expect("the report is UTF-8");
        assert_eq!(
            text,
            "Kernel panic: panicked at kernel/src/lib.rs:1:1:\nthe machine gave up\n"
        );
    }
}
