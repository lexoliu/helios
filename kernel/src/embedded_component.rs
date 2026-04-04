/// Raw WebAssembly component bytes embedded into the binary.
///
/// The kernel keeps the payload opaque. Higher layers decide whether the
/// component is launched as `init`, a driver, or some other user-mode payload,
/// and JIT-compile it when needed.
#[derive(Clone, Copy)]
pub struct EmbeddedComponent {
    name: &'static str,
    bytes: &'static [u8],
}

impl EmbeddedComponent {
    pub const fn new(name: &'static str, bytes: &'static [u8]) -> Self {
        Self { name, bytes }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}
