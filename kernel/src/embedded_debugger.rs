use crate::EmbeddedComponent;

#[derive(Clone, Copy)]
pub struct EmbeddedDebugger {
    component: EmbeddedComponent,
}

impl EmbeddedDebugger {
    pub const fn component(&self) -> EmbeddedComponent {
        self.component
    }
}

pub fn embedded_debugger() -> Option<EmbeddedDebugger> {
    generated::EMBEDDED_DEBUGGER.map(|descriptor| EmbeddedDebugger {
        component: descriptor.component,
    })
}

#[derive(Clone, Copy)]
pub struct EmbeddedDebuggerDescriptor {
    component: EmbeddedComponent,
}

mod generated {
    #[allow(unused_imports)]
    use super::{EmbeddedComponent, EmbeddedDebuggerDescriptor};

    include!(concat!(env!("OUT_DIR"), "/embedded_debugger.rs"));
}
