extern crate alloc;

use alloc::vec::Vec;

use helios_hal::serial::ByteSerial;

use super::console::emit_console_line;

pub type SerialReader = fn(&mut Vec<u8>, u32);

/// Try a non-blocking read into caller-owned storage. Returns whatever bytes
/// are immediately available up to `max_bytes`. Leaves `buffer` empty when no
/// byte is ready. Callers that need to wait for a byte should loop with
/// `yield_now().await` between polls rather than spinning on the port.
pub fn try_read_serial(io: &impl ByteSerial, buffer: &mut Vec<u8>, max_bytes: u32) {
    buffer.clear();
    let max_bytes = max_bytes as usize;
    while buffer.len() < max_bytes {
        let Some(byte) = io.try_read_byte() else {
            break;
        };
        buffer.push(byte);
    }
}

/// Synchronous blocking read used only during bootstrap (before the
/// kernel executor is active). Spins on the port until a byte is available.
pub fn read_serial(io: &impl ByteSerial, buffer: &mut Vec<u8>, max_bytes: u32) {
    buffer.clear();
    let max_bytes = max_bytes as usize;

    loop {
        if let Some(byte) = io.try_read_byte() {
            buffer.push(byte);
            break;
        }
        core::hint::spin_loop();
    }

    while buffer.len() < max_bytes {
        let Some(byte) = io.try_read_byte() else {
            break;
        };
        buffer.push(byte);
    }
}

pub fn write_serial(io: &impl ByteSerial, bytes: &[u8]) {
    emit_console_line(|| io.write_bytes(bytes));
}

pub fn emit_serial_stage_marker(io: &impl ByteSerial, stage: &str) {
    emit_console_line(|| {
        io.write_bytes(b"\n[KDBG ");
        io.write_bytes(stage.as_bytes());
        io.write_bytes(b"]\n");
    });
}

pub fn emit_serial_error_marker(io: &impl ByteSerial, label: &str, message: &str) {
    emit_console_line(|| {
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
    });
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

        let mut bytes = Vec::new();
        try_read_serial(&serial, &mut bytes, u32::MAX);

        assert!(bytes.is_empty());
        assert_eq!(bytes.capacity(), 0);
    }
}
