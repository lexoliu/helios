use std::fmt::{self, Write};
use std::io::{self, Write as _};

/// Hosted console bridge used by the kernel logger.
///
/// The kernel already serializes accesses around this writer, so the hosted
/// console only needs to forward bytes into stdout and flush so logs appear
/// promptly while debugging.
pub struct HostedConsole {
    stdout: io::Stdout,
}

impl HostedConsole {
    pub fn new() -> Self {
        Self {
            stdout: io::stdout(),
        }
    }
}

impl Write for HostedConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.stdout
            .write_all(s.as_bytes())
            .and_then(|_| self.stdout.flush())
            .map_err(|_| fmt::Error)
    }
}
