mod config;
mod console;
mod cpu;
mod debugger_bindings;
mod init_program;
mod observer_buffer;
mod observer_host;
mod program_bindings;
mod runtime;
mod serial_host;
mod sync_host;

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
