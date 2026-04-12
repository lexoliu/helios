use std::fmt::{self, Write};
use std::io::{self, Write as _};

/// Hosted console bridge used by the kernel logger.
///
/// The kernel already serializes accesses around this writer, so the hosted
/// console only needs to forward bytes into stderr and flush so logs stay
/// visible without corrupting the debugger serial protocol on stdout.
pub struct HostedConsole {
    stderr: io::Stderr,
}

impl HostedConsole {
    pub fn new() -> Self {
        Self {
            stderr: io::stderr(),
        }
    }
}

impl Write for HostedConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.stderr
            .write_all(s.as_bytes())
            .and_then(|_| self.stderr.flush())
            .map_err(|_| fmt::Error)
    }
}
