/// Target-specific precompiled wasm artifact embedded into the binary.
///
/// Higher layers decide when to load an embedded program. The kernel only
/// carries opaque AOT bytes plus a small amount of identity metadata.
#[derive(Clone, Copy)]
pub struct EmbeddedProgram {
    name: &'static str,
    target: &'static str,
    artifact: &'static [u8],
}

impl EmbeddedProgram {
    pub const fn new(name: &'static str, target: &'static str, artifact: &'static [u8]) -> Self {
        Self {
            name,
            target,
            artifact,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn target(&self) -> &'static str {
        self.target
    }

    pub const fn artifact(&self) -> &'static [u8] {
        self.artifact
    }
}
