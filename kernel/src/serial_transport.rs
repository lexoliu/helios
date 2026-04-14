extern crate alloc;

use alloc::vec::Vec;

use helios_hal::serial::ByteSerial;

/// Try a non-blocking read — returns whatever bytes are immediately
/// available up to `max_bytes`. Returns an empty `Vec` when no byte is
/// ready. Callers that need to wait for a byte should loop with
/// `yield_now().await` between polls rather than spinning on the port.
pub fn try_read_serial(io: &impl ByteSerial, max_bytes: u32) -> Vec<u8> {
    let max_bytes = max_bytes as usize;
    let mut bytes = Vec::with_capacity(max_bytes);
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
    let mut bytes = Vec::with_capacity(max_bytes);

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
