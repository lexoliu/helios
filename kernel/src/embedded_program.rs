/// Trusted precompiled component bytes embedded into the binary.
///
/// Higher layers decide when to launch an embedded program. The kernel keeps
/// the payload opaque and deserializes it through the trusted artifact loader.
#[derive(Clone, Copy)]
pub struct EmbeddedProgram {
    name: &'static str,
    bytes: &'static [u8],
}

impl EmbeddedProgram {
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
