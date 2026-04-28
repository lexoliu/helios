mod compaction_tests;
mod config;
mod console;
mod cpu;
mod host_fs;
mod oom_killer_tests;
mod pmm_tests;
mod runtime;
mod vmm;

pub use vmm::HostedAddressSpace;

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::any::Any;
use std::fmt;
use std::panic::PanicHookInfo;

use config::HostedConfig;
use runtime::HostedRuntime;

pub fn main() {
    install_panic_hook();
    HostedRuntime::new(HostedConfig::from_env()).run();
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        helios_kernel::panic_log_message(PanicPayload(info.payload()), info.location());
    }));
}

struct PanicPayload<'a>(&'a (dyn Any + Send));

impl fmt::Display for PanicPayload<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(message) = self.0.downcast_ref::<&str>() {
            return f.write_str(message);
        }

        if let Some(message) = self.0.downcast_ref::<String>() {
            return f.write_str(message);
        }

        f.write_str("non-string panic payload")
    }
}

#[allow(dead_code)]
fn _panic_hook_typecheck(_: &PanicHookInfo<'_>) {}
