use crate::{EmbeddedBootFs, EmbeddedComponent};

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
}

pub fn embedded_init() -> Option<EmbeddedInit> {
    generated::EMBEDDED_INIT.map(|descriptor| EmbeddedInit {
        component: descriptor.component,
        argv0: descriptor.argv0,
        bootfs: descriptor.bootfs,
    })
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
    use crate::EmbeddedBootFile;

    include!(concat!(env!("OUT_DIR"), "/embedded_init.rs"));
}
