/// Target-specific precompiled WebAssembly component embedded into the binary.
///
/// The kernel keeps the artifact opaque. Higher layers decide whether the
/// component is launched as `init`, a driver, or some other user-mode payload.
#[derive(Clone, Copy)]
pub struct EmbeddedComponent {
    name: &'static str,
    target: &'static str,
    artifact: &'static [u8],
}

impl EmbeddedComponent {
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
