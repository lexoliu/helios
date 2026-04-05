extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use wasmtime::Result;
use wasmtime::component::{HasSelf, Linker, Resource};
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::{DynPollable, Pollable, subscribe};
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream, InputStream, StreamError};

use crate::debugger_program::{DeadlinePollable, DebugSerialOutputStream, StoreData};

pub(crate) mod cli_bindings {
    mod generated {
        wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-cli;

                world imports {
                    import wasi:cli/environment@0.2.6;
                    import wasi:cli/exit@0.2.6;
                    import wasi:cli/stdin@0.2.6;
                    import wasi:cli/stdout@0.2.6;
                    import wasi:cli/stderr@0.2.6;
                    import wasi:cli/terminal-input@0.2.6;
                    import wasi:cli/terminal-output@0.2.6;
                    import wasi:cli/terminal-stdin@0.2.6;
                    import wasi:cli/terminal-stdout@0.2.6;
                    import wasi:cli/terminal-stderr@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            with: {
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
                "wasi:io/streams.input-stream": wasmtime_wasi_io::streams::DynInputStream,
                "wasi:io/streams.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
                "wasi:cli/terminal-input.terminal-input": crate::debugger_wasi_p2::TerminalInput,
                "wasi:cli/terminal-output.terminal-output": crate::debugger_wasi_p2::TerminalOutput,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod clocks_bindings {
    mod generated {
        wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-clocks;

                world imports {
                    import wasi:clocks/monotonic-clock@0.2.6;
                    import wasi:clocks/wall-clock@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            with: {
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod random_bindings {
    mod generated {
        wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-random;

                world imports {
                    import wasi:random/random@0.2.6;
                    import wasi:random/insecure@0.2.6;
                    import wasi:random/insecure-seed@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub struct TerminalInput;
pub struct TerminalOutput;

struct EmptyInputStream;

pub(crate) fn add_to_linker(linker: &mut Linker<StoreData>) -> Result<()> {
    type Data = HasSelf<StoreData>;

    cli_bindings::cli::environment::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::exit::add_to_linker::<_, Data>(linker, &Default::default(), |state| state)?;
    cli_bindings::cli::stdin::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::stdout::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::stderr::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_input::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_output::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_stdin::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_stdout::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_stderr::add_to_linker::<_, Data>(linker, |state| state)?;

    clocks_bindings::clocks::monotonic_clock::add_to_linker::<_, Data>(linker, |state| state)?;
    clocks_bindings::clocks::wall_clock::add_to_linker::<_, Data>(linker, |state| state)?;

    random_bindings::random::random::add_to_linker::<_, Data>(linker, |state| state)?;
    random_bindings::random::insecure::add_to_linker::<_, Data>(linker, |state| state)?;
    random_bindings::random::insecure_seed::add_to_linker::<_, Data>(linker, |state| state)?;
    Ok(())
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for EmptyInputStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for EmptyInputStream {
    fn read(&mut self, _: usize) -> core::result::Result<Bytes, StreamError> {
        Ok(Bytes::new())
    }
}

impl cli_bindings::cli::environment::Host for StoreData {
    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment.clone())
    }

    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments.clone())
    }

    fn initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
}

impl cli_bindings::cli::exit::Host for StoreData {
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let message = match status {
            Ok(()) => "guest requested wasi p2 exit success",
            Err(()) => "guest requested wasi p2 exit failure",
        };
        Err(wasmtime::Error::msg(message))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi p2 exit code {status_code}"
        )))
    }
}

impl cli_bindings::cli::stdin::Host for StoreData {
    fn get_stdin(&mut self) -> Result<Resource<DynInputStream>> {
        Ok(self
            .table
            .push(Box::new(EmptyInputStream) as DynInputStream)?)
    }
}

impl cli_bindings::cli::stdout::Host for StoreData {
    fn get_stdout(&mut self) -> Result<Resource<DynOutputStream>> {
        Ok(self
            .table
            .push(Box::new(DebugSerialOutputStream::from_store(self)) as DynOutputStream)?)
    }
}

impl cli_bindings::cli::stderr::Host for StoreData {
    fn get_stderr(&mut self) -> Result<Resource<DynOutputStream>> {
        Ok(self
            .table
            .push(Box::new(DebugSerialOutputStream::from_store(self)) as DynOutputStream)?)
    }
}

impl cli_bindings::cli::terminal_input::Host for StoreData {}
impl cli_bindings::cli::terminal_output::Host for StoreData {}

impl cli_bindings::cli::terminal_input::HostTerminalInput for StoreData {
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl cli_bindings::cli::terminal_output::HostTerminalOutput for StoreData {
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl cli_bindings::cli::terminal_stdin::Host for StoreData {
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl cli_bindings::cli::terminal_stdout::Host for StoreData {
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl cli_bindings::cli::terminal_stderr::Host for StoreData {
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl clocks_bindings::clocks::monotonic_clock::Host for StoreData {
    fn now(&mut self) -> Result<clocks_bindings::clocks::monotonic_clock::Instant> {
        Ok(self.now_nanos())
    }

    fn resolution(&mut self) -> Result<clocks_bindings::clocks::monotonic_clock::Duration> {
        Ok(1)
    }

    fn subscribe_instant(&mut self, when: u64) -> Result<Resource<DynPollable>> {
        let resource = self.table.push(DeadlinePollable {
            cpu: self.cpu.clone(),
            deadline_nanos: when,
        })?;
        subscribe(&mut self.table, resource)
    }

    fn subscribe_duration(&mut self, when: u64) -> Result<Resource<DynPollable>> {
        self.subscribe_instant(self.now_nanos().saturating_add(when))
    }
}

impl clocks_bindings::clocks::wall_clock::Host for StoreData {
    fn now(&mut self) -> Result<clocks_bindings::clocks::wall_clock::Datetime> {
        Ok(system_time_from_nanos(self.now_nanos()))
    }

    fn resolution(&mut self) -> Result<clocks_bindings::clocks::wall_clock::Datetime> {
        Ok(clocks_bindings::clocks::wall_clock::Datetime {
            seconds: 0,
            nanoseconds: 1,
        })
    }
}

impl random_bindings::random::random::Host for StoreData {
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0; len as usize])
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl random_bindings::random::insecure::Host for StoreData {
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0; len as usize])
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl random_bindings::random::insecure_seed::Host for StoreData {
    fn insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }
}

fn system_time_from_nanos(nanos: u64) -> clocks_bindings::clocks::wall_clock::Datetime {
    clocks_bindings::clocks::wall_clock::Datetime {
        seconds: nanos / 1_000_000_000,
        nanoseconds: (nanos % 1_000_000_000) as u32,
    }
}
