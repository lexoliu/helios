extern crate alloc;

use alloc::vec::Vec;

use helios_hal::serial::ByteSerial;

/// Try a non-blocking read — returns whatever bytes are immediately
/// available up to `max_bytes`. Returns an empty `Vec` when no byte is
/// ready. Callers that need to wait for a byte should loop with
/// `yield_now().await` between polls rather than spinning on the port.
pub fn try_read_serial(io: &impl ByteSerial, max_bytes: u32) -> Vec<u8> {
    let max_bytes = max_bytes as usize;
    // `max_bytes` may come from a guest fd-read capacity or from "drain
    // whatever is ready" callers. Do not reserve that bound up front; UART
    // readiness is discovered one byte at a time, so allocating from the bound
    // turns a harmless empty poll into a huge allocation request.
    let mut bytes = Vec::new();
    while bytes.len() < max_bytes {
        let Some(byte) = io.try_read_byte() else {
            break;
        };
        bytes.push(byte);
    }
    bytes
}

/// Synchronous blocking read used only during bootstrap (before the
/// kernel executor is active). Spins on the port until a byte is available.
pub fn read_serial(io: &impl ByteSerial, max_bytes: u32) -> Vec<u8> {
    let max_bytes = max_bytes as usize;
    let mut bytes = Vec::new();

    loop {
        if let Some(byte) = io.try_read_byte() {
            bytes.push(byte);
            break;
        }
        core::hint::spin_loop();
    }

    while bytes.len() < max_bytes {
        let Some(byte) = io.try_read_byte() else {
            break;
        };
        bytes.push(byte);
    }

    bytes
}

pub fn write_serial(io: &impl ByteSerial, bytes: &[u8]) {
    io.write_bytes(bytes);
}

pub fn emit_serial_stage_marker(io: &impl ByteSerial, stage: &str) {
    io.write_bytes(b"\n[KDBG ");
    io.write_bytes(stage.as_bytes());
    io.write_bytes(b"]\n");
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestSerial {
        bytes: &'static [u8],
        cursor: AtomicUsize,
    }

    impl TestSerial {
        const fn new(bytes: &'static [u8]) -> Self {
            Self {
                bytes,
                cursor: AtomicUsize::new(0),
            }
        }
    }

    impl ByteSerial for TestSerial {
        fn try_read_byte(&self) -> Option<u8> {
            let index = self.cursor.fetch_add(1, Ordering::Relaxed);
            self.bytes.get(index).copied()
        }

        fn write_bytes(&self, _bytes: &[u8]) {}
    }

    #[test]
    fn unbounded_try_read_does_not_preallocate_when_idle() {
        let serial = TestSerial::new(&[]);

        let bytes = try_read_serial(&serial, u32::MAX);

        assert!(bytes.is_empty());
        assert_eq!(bytes.capacity(), 0);
    }
}

pub fn emit_serial_error_marker(io: &impl ByteSerial, label: &str, message: &str) {
    io.write_bytes(b"\n[KDBG ");
    io.write_bytes(label.as_bytes());
    io.write_bytes(b": ");
    for byte in message.bytes() {
        match byte {
            b'\n' | b'\r' => io.write_bytes(b" "),
            b']' => io.write_bytes(b")"),
            other => io.write_bytes(&[other]),
        }
    }
    io.write_bytes(b"]\n");
}
