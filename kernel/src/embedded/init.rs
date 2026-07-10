use crate::{EmbeddedBootFs, EmbeddedComponent};

const EMBEDDED_SYSTEM_COMPONENT_PATH: &str = "bin/debugger";

pub struct EmbeddedInit {
    component: EmbeddedComponent,
    argv0: &'static str,
    bootfs: EmbeddedBootFs,
}

impl EmbeddedInit {
    pub const fn component(&self) -> EmbeddedComponent {
        self.component
    }

    pub const fn argv0(&self) -> &'static str {
        self.argv0
    }

    pub const fn bootfs(&self) -> EmbeddedBootFs {
        self.bootfs
    }

    pub fn boot_component(&self, path: &str) -> Option<EmbeddedComponent> {
        self.bootfs
            .files()
            .iter()
            .find(|file| file.path() == path)
            .map(|file| EmbeddedComponent::new(file.path(), file.contents()))
    }
}

pub fn embedded_init() -> Option<EmbeddedInit> {
    generated::EMBEDDED_INIT.map(|descriptor| EmbeddedInit {
        component: descriptor.component,
        argv0: descriptor.argv0,
        bootfs: descriptor.bootfs,
    })
}

pub fn embedded_boot_component(path: &str) -> Option<EmbeddedComponent> {
    embedded_init().and_then(|init| init.boot_component(path))
}

/// The embedded debugger system component, when the `embedded-debugger`
/// feature is enabled and the prebuild bootfs carries it. Kernels built
/// without the feature never autostart a system component.
pub fn embedded_system_component() -> Option<EmbeddedComponent> {
    if cfg!(feature = "embedded-debugger") {
        embedded_boot_component(EMBEDDED_SYSTEM_COMPONENT_PATH)
    } else {
        None
    }
}

pub fn has_embedded_system_component() -> bool {
    embedded_system_component().is_some()
}

#[derive(Clone, Copy)]
pub struct EmbeddedInitDescriptor {
    component: EmbeddedComponent,
    argv0: &'static str,
    bootfs: EmbeddedBootFs,
}

mod generated {
    #[allow(unused_imports)]
    use super::{EmbeddedBootFs, EmbeddedComponent, EmbeddedInitDescriptor};
    #[allow(unused_imports)]
    use crate::{EmbeddedBootDirectory, EmbeddedBootFile};

    include!(concat!(env!("OUT_DIR"), "/embedded_init.rs"));
}
