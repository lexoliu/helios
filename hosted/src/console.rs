use std::fmt::{self, Write};
use std::io::{self, Write as _};

use crate::observer_buffer::SharedObserverBuffer;

/// Hosted console bridge used by the kernel logger.
///
/// The kernel already serializes accesses around this writer, so the hosted
/// console only needs to forward bytes into stderr and flush so logs stay
/// visible without corrupting the debugger serial protocol on stdout.
pub struct HostedConsole {
    stderr: io::Stderr,
    observer: SharedObserverBuffer,
}

impl HostedConsole {
    pub fn new(observer: SharedObserverBuffer) -> Self {
        Self {
            stderr: io::stderr(),
            observer,
        }
    }
}

impl Write for HostedConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.observer.record_console_text(s);
        self.stderr
            .write_all(s.as_bytes())
            .and_then(|_| self.stderr.flush())
            .map_err(|_| fmt::Error)
    }
}
