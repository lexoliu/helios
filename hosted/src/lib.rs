use std::fmt::{self, Write};

use helios_hal::Platform;
use helios_kernel::init;

const HOSTED_HEAP_SIZE: usize = 16 * 1024 * 1024;

struct StdoutConsole;

impl Write for StdoutConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        use std::io::Write as _;

        let mut stdout = std::io::stdout().lock();
        stdout.write_all(s.as_bytes()).map_err(|_| fmt::Error)?;
        stdout.flush().map_err(|_| fmt::Error)
    }
}

pub fn main() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("non-string panic payload");
        helios_kernel::panic_log_message(message, info.location());
    }));

    let heap = vec![0; HOSTED_HEAP_SIZE].into_boxed_slice();
    let heap = Box::leak(heap);
    init(Platform::new(StdoutConsole, heap));
}
