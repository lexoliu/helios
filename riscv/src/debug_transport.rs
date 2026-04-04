use fdt::Fdt;
use ns16550a::Uart;

/// Debugger byte transport backed by the machine's boot UART. Kernel tracing
/// stays in memory so the line remains reserved for RPC traffic after boot.
pub(crate) struct DebugTransport {
    uart_base: usize,
}

impl DebugTransport {
    pub(crate) fn discover(fdt: &Fdt<'_>) -> Option<Self> {
        let chosen = fdt.find_node("/chosen")?;
        let stdout_path = chosen
            .properties()
            .find(|property| property.name == "stdin-path")
            .or_else(|| {
                chosen
                    .properties()
                    .find(|property| property.name == "stdout-path")
            })?;
        let path = core::str::from_utf8(stdout_path.value)
            .ok()?
            .trim_end_matches('\0')
            .split(':')
            .next()?;
        let node = fdt
            .find_node(path)
            .or_else(|| fdt.aliases().and_then(|aliases| aliases.resolve_node(path)))?;
        let region = node.reg()?.next()?;
        Some(Self {
            uart_base: region.starting_address as usize,
        })
    }

    pub(crate) fn try_read_byte(&self) -> Option<u8> {
        Uart::new(self.uart_base).get()
    }

    pub(crate) fn write_bytes(&self, bytes: &[u8]) {
        let uart = Uart::new(self.uart_base);
        for &byte in bytes {
            while uart.put(byte).is_none() {
                core::hint::spin_loop();
            }
        }
    }
}
