use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use wasmtime::component::{HasSelf, Linker, Resource};
use wasmtime::{self, Result};
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::{DynPollable, Pollable, subscribe};
use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, Error as IoError, InputStream, StreamError,
};
use wasmtime_wasi_io::{self};

use crate::component_host::{RuntimeDeadlinePollable, StoreData};
use crate::wasmtime_adapter::store::{
    ChannelInputStream, ChannelOutputStream, StdioOutputStream,
};
use crate::wasmtime_adapter::wasi::map_host_fs_error;
use crate::{ComponentOutputMode, ComponentOutputStreamKind};
use super::bindings::filesystem::types as p3fs;
use super::{FsDescriptor, FsNodeKind, P2Network, TcpSocket, UdpSocket};

pub(crate) mod cli_bindings {
    mod generated {
        use wasmtime;
        use wasmtime_wasi_io;

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
                "wasi:cli/terminal-input.terminal-input": crate::wasmtime_adapter::wasi::p2::TerminalInput,
                "wasi:cli/terminal-output.terminal-output": crate::wasmtime_adapter::wasi::p2::TerminalOutput,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod clocks_bindings {
    mod generated {
        use wasmtime;
        use wasmtime_wasi_io;

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

pub(crate) mod filesystem_bindings {
    mod generated {
        use wasmtime;
        use wasmtime_wasi_io;

        wasmtime::component::bindgen!({
            inline: "
                    package helios:debugger-p2-filesystem;

                    world imports {
                        import wasi:filesystem/preopens@0.2.6;
                        import wasi:filesystem/types@0.2.6;
                    }
                ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: {
                "wasi:filesystem/types.[method]descriptor.stat": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.stat-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.open-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.read-directory": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.create-directory-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.unlink-file-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.rename-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.remove-directory-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.metadata-hash": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.metadata-hash-at": async | tracing | trappable,
                default: tracing | trappable,
            },
            with: {
                "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
                "wasi:io/streams.input-stream": wasmtime_wasi_io::streams::DynInputStream,
                "wasi:io/streams.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
                "wasi:filesystem/types.descriptor": crate::wasmtime_adapter::wasi::FsDescriptor,
                "wasi:filesystem/types.directory-entry-stream": crate::wasmtime_adapter::wasi::p2::DirectoryEntryStream,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod random_bindings {
    mod generated {
        use wasmtime;

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

pub(crate) mod sockets_bindings {
    mod generated {
        use wasmtime;
        use wasmtime_wasi_io;

        wasmtime::component::bindgen!({
            inline: "
                    package helios:debugger-p2-sockets;

                    world imports {
                        import wasi:sockets/network@0.2.6;
                        import wasi:sockets/instance-network@0.2.6;
                        import wasi:sockets/udp@0.2.6;
                        import wasi:sockets/udp-create-socket@0.2.6;
                        import wasi:sockets/tcp@0.2.6;
                        import wasi:sockets/tcp-create-socket@0.2.6;
                        import wasi:sockets/ip-name-lookup@0.2.6;
                    }
                ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            with: {
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
                "wasi:io/streams.input-stream": wasmtime_wasi_io::streams::DynInputStream,
                "wasi:io/streams.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
                "wasi:sockets/network.network": crate::wasmtime_adapter::wasi::P2Network,
                "wasi:sockets/tcp.tcp-socket": crate::wasmtime_adapter::wasi::TcpSocket,
                "wasi:sockets/udp.udp-socket": crate::wasmtime_adapter::wasi::UdpSocket,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

use filesystem_bindings::filesystem::types as p2fs;
use sockets_bindings::io::error as p2io_error;
use sockets_bindings::sockets::ip_name_lookup as p2lookup;
use sockets_bindings::sockets::network as p2net;
use sockets_bindings::sockets::tcp as p2tcp;
use sockets_bindings::sockets::tcp_create_socket as p2tcp_create;
use sockets_bindings::sockets::udp as p2udp;
use sockets_bindings::sockets::udp_create_socket as p2udp_create;

pub struct TerminalInput;
pub struct TerminalOutput;
pub struct DirectoryEntryStream {
    entries: Vec<p2fs::DirectoryEntry>,
    cursor: usize,
}

struct EmptyInputStream;

struct FileInputStream {
    bytes: Vec<u8>,
    cursor: usize,
}

impl FileInputStream {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, cursor: 0 }
    }
}

pub(crate) fn add_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{

    cli_bindings::cli::environment::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::exit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, &Default::default(), |state| state)?;
    cli_bindings::cli::stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::terminal_input::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::terminal_output::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::terminal_stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::terminal_stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    cli_bindings::cli::terminal_stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;

    clocks_bindings::clocks::monotonic_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    clocks_bindings::clocks::wall_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;

    filesystem_bindings::filesystem::preopens::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    filesystem_bindings::filesystem::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;

    random_bindings::random::random::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    random_bindings::random::insecure::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    random_bindings::random::insecure_seed::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    sockets_bindings::sockets::network::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
        linker,
        &Default::default(),
        |state| state,
    )?;
    sockets_bindings::sockets::instance_network::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    sockets_bindings::sockets::udp::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    sockets_bindings::sockets::udp_create_socket::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    sockets_bindings::sockets::tcp::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    sockets_bindings::sockets::tcp_create_socket::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    sockets_bindings::sockets::ip_name_lookup::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
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

#[wasmtime_wasi_io::async_trait]
impl Pollable for FileInputStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for FileInputStream {
    fn read(&mut self, size: usize) -> core::result::Result<Bytes, StreamError> {
        if self.cursor >= self.bytes.len() {
            return Err(StreamError::Closed);
        }

        let end = self.cursor.saturating_add(size).min(self.bytes.len());
        let chunk = Bytes::copy_from_slice(&self.bytes[self.cursor..end]);
        self.cursor = end;
        Ok(chunk)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::environment::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment().to_vec())
    }

    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments().to_vec())
    }

    fn initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(Some(String::from("/")))
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::exit::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let code = match status {
            Ok(()) => 0,
            Err(()) => 1,
        };
        self.request_exit(code);
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi p2 exit code {code}"
        )))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        self.request_exit(status_code);
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi p2 exit code {status_code}"
        )))
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_stdin(&mut self) -> Result<Resource<DynInputStream>> {
        let stream: DynInputStream = match self.output_mode() {
            ComponentOutputMode::Child { stdin_rx, .. } => {
                Box::new(ChannelInputStream::new(stdin_rx.clone())) as DynInputStream
            }
            ComponentOutputMode::Serial | ComponentOutputMode::Trace => {
                Box::new(EmptyInputStream) as DynInputStream
            }
        };
        Ok(self.table.push(stream)?)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_stdout(&mut self) -> Result<Resource<DynOutputStream>> {
        let stream = build_stdio_stream(self, ComponentOutputStreamKind::Stdout);
        Ok(self.table.push(Box::new(stream) as DynOutputStream)?)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_stderr(&mut self) -> Result<Resource<DynOutputStream>> {
        let stream = build_stdio_stream(self, ComponentOutputStreamKind::Stderr);
        Ok(self.table.push(Box::new(stream) as DynOutputStream)?)
    }
}

fn build_stdio_stream<CpuImpl, HostFs>(
    store: &StoreData<CpuImpl, HostFs>,
    kind: ComponentOutputStreamKind,
) -> StdioOutputStream
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    match store.output_mode() {
        ComponentOutputMode::Child { .. } => {
            let writer = store
                .output_mode()
                .child_writer(kind)
                .expect("child mode always has stdout/stderr writers");
            StdioOutputStream::Child(ChannelOutputStream::new(writer))
        }
        ComponentOutputMode::Serial => {
            StdioOutputStream::Serial(store.serial_writer_fn())
        }
        ComponentOutputMode::Trace => StdioOutputStream::Trace,
    }
}


impl<CpuImpl, HostFs> cli_bindings::cli::terminal_input::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}
impl<CpuImpl, HostFs> cli_bindings::cli::terminal_output::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_input::HostTerminalInput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_output::HostTerminalOutput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> clocks_bindings::clocks::monotonic_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<clocks_bindings::clocks::monotonic_clock::Instant> {
        Ok(self.now_nanos())
    }

    fn resolution(&mut self) -> Result<clocks_bindings::clocks::monotonic_clock::Duration> {
        Ok(1)
    }

    fn subscribe_instant(&mut self, when: u64) -> Result<Resource<DynPollable>> {
        let resource = self.table.push(RuntimeDeadlinePollable::new(
            self.cpu.clone(),
            self.runtime_state.clone(),
            when,
        ))?;
        subscribe(&mut self.table, resource)
    }

    fn subscribe_duration(&mut self, when: u64) -> Result<Resource<DynPollable>> {
        self.subscribe_instant(self.now_nanos().saturating_add(when))
    }
}

impl<CpuImpl, HostFs> clocks_bindings::clocks::wall_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
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

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::preopens::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_directories(&mut self) -> Result<Vec<(Resource<FsDescriptor>, String)>> {
        let descriptor = self.filesystem().root_descriptor();
        let resource = self.table.push(descriptor)?;
        Ok(vec![(resource, String::from("/"))])
    }
}

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn filesystem_error_code(&mut self, _: Resource<IoError>) -> Result<Option<p2fs::ErrorCode>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::types::HostDescriptor for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<core::result::Result<Resource<DynInputStream>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let max_bytes = match self.filesystem_mut().stat(&descriptor.path) {
            Ok(stat) => {
                if offset >= stat.size {
                    0
                } else {
                    let remaining = stat.size.saturating_sub(offset);
                    match usize::try_from(remaining) {
                        Ok(remaining) => remaining,
                        Err(_) => return Ok(Err(p2fs::ErrorCode::Overflow)),
                    }
                }
            }
            Err(error) => return Ok(Err(error_code_from_p3(error))),
        };
        match self
            .filesystem_mut()
            .read_file_chunk(&descriptor, offset, max_bytes)
        {
            Ok(bytes) => {
                let resource = self
                    .table
                    .push(Box::new(FileInputStream::new(bytes)) as DynInputStream)?;
                Ok(Ok(resource))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn write_via_stream(
        &mut self,
        _: Resource<FsDescriptor>,
        _: u64,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn append_via_stream(
        &mut self,
        _: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn advise(
        &mut self,
        _: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: p2fs::Advice,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn sync_data(
        &mut self,
        _: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Ok(()))
    }

    fn get_flags(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorFlags, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        Ok(Ok(descriptor_flags_from_p3(descriptor.flags)))
    }

    fn get_type(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorType, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        Ok(Ok(descriptor_type_from_kind(descriptor.kind)))
    }

    fn set_size(
        &mut self,
        _: Resource<FsDescriptor>,
        _: u64,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn set_times(
        &mut self,
        _: Resource<FsDescriptor>,
        _: p2fs::NewTimestamp,
        _: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn read(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        length: u64,
        offset: u64,
    ) -> Result<core::result::Result<(Vec<u8>, bool), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let length: usize = match length.try_into() {
            Ok(length) => length,
            Err(_) => return Ok(Err(p2fs::ErrorCode::Overflow)),
        };
        match self.filesystem_mut().read_file_chunk(&descriptor, offset, length) {
            Ok(bytes) => {
                let eof = bytes.len() < length;
                Ok(Ok((bytes, eof)))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn write(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        buffer: Vec<u8>,
        offset: u64,
    ) -> Result<core::result::Result<u64, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let offset: usize = match offset.try_into() {
            Ok(offset) => offset,
            Err(_) => return Ok(Err(p2fs::ErrorCode::Overflow)),
        };
        let now_nanos = self.now_nanos();
        match self
            .filesystem_mut()
            .write_at(&descriptor, offset, &buffer, now_nanos)
        {
            Ok(()) => Ok(Ok(buffer.len() as u64)),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn read_directory(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DirectoryEntryStream>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        // For host-mirrored paths, pull the entries through the async
        // host-fs service first so the sync embedded path sees a fully
        // populated view.
        if let Some(host_path) = crate::guest_host_share_path(&descriptor.path) {
            if let Ok(service) = self
                .filesystem()
                .host_service()
                .map_err(error_code_from_p3)
            {
                let host_path = host_path.to_owned();
                match service.read_dir(&host_path).await {
                    Ok(entries) => {
                        self.filesystem_mut()
                            .seed_host_directory_entries(&descriptor.path, entries);
                    }
                    Err(err) => return Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
                }
            }
        }
        match self.filesystem_mut().read_directory(&descriptor.path) {
            Ok(entries) => {
                let resource = self.table.push(DirectoryEntryStream {
                    entries: entries.into_iter().map(directory_entry_from_p3).collect(),
                    cursor: 0,
                })?;
                Ok(Ok(resource))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn sync(
        &mut self,
        _: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Ok(()))
    }

    async fn create_directory_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.create_directory(&host_path).await {
                Ok(()) => {
                    self.filesystem_mut().invalidate_host_subtree(&absolute);
                    Ok(Ok(()))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        let now_nanos = self.now_nanos();
        match self
            .filesystem_mut()
            .create_directory_at(&descriptor, &path, now_nanos)
        {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn stat(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorStat, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if let Some(host_path) = crate::guest_host_share_path(&descriptor.path) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.stat_path(&host_path).await {
                Ok(metadata) => Ok(Ok(p2fs::DescriptorStat {
                    type_: if metadata.qid_type & 0x80 != 0 {
                        p2fs::DescriptorType::Directory
                    } else {
                        p2fs::DescriptorType::RegularFile
                    },
                    link_count: 1,
                    size: metadata.size,
                    data_access_timestamp: None,
                    data_modification_timestamp: None,
                    status_change_timestamp: None,
                })),
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem_mut().stat(&descriptor.path) {
            Ok(stat) => Ok(Ok(descriptor_stat_from_p3(stat))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn stat_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
    ) -> Result<core::result::Result<p2fs::DescriptorStat, p2fs::ErrorCode>> {
        let _ = path_flags;
        self.write_serial(alloc::format!("[p2:stat_at] path={path:?}\n").as_bytes());
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let path = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&path) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.stat_path(&host_path).await {
                Ok(metadata) => Ok(Ok(p2fs::DescriptorStat {
                    type_: if metadata.qid_type & 0x80 != 0 {
                        p2fs::DescriptorType::Directory
                    } else {
                        p2fs::DescriptorType::RegularFile
                    },
                    link_count: 1,
                    size: metadata.size,
                    data_access_timestamp: None,
                    data_modification_timestamp: None,
                    status_change_timestamp: None,
                })),
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem_mut().stat(&path) {
            Ok(stat) => Ok(Ok(descriptor_stat_from_p3(stat))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn set_times_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        _: String,
        _: p2fs::NewTimestamp,
        _: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn link_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        _: String,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    async fn open_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
        open_flags: p2fs::OpenFlags,
        flags: p2fs::DescriptorFlags,
    ) -> Result<core::result::Result<Resource<FsDescriptor>, p2fs::ErrorCode>> {
        let _ = path_flags;
        self.write_serial(alloc::format!("[p2:open_at] path={path:?}\n").as_bytes());
        let base = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if base.kind != crate::wasmtime_adapter::wasi::FsNodeKind::Directory {
            return Ok(Err(p2fs::ErrorCode::NotDirectory));
        }
        let absolute = match crate::resolve_child_path(&base.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        let open_flags_p3 = open_flags_to_p3(open_flags);
        let descriptor_flags_p3 = descriptor_flags_to_p3(flags);
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            let metadata = match service.stat_path(&host_path).await {
                Ok(meta) => Some(meta),
                Err(err) => {
                    self.write_serial(
                        alloc::format!(
                            "[p2:open_at] host stat failed path={host_path:?} kind={:?}\n",
                            err.kind()
                        )
                        .as_bytes(),
                    );
                    let code = map_host_fs_error(err);
                    if matches!(
                        code,
                        crate::wasmtime_adapter::wasi::bindings::filesystem::types::ErrorCode::NoEntry
                    ) && open_flags_p3.contains(
                        crate::wasmtime_adapter::wasi::bindings::filesystem::types::OpenFlags::CREATE,
                    ) {
                        None
                    } else {
                        return Ok(Err(error_code_from_p3(code)));
                    }
                }
            };
            let (kind, contents) = if let Some(meta) = metadata {
                let is_dir = meta.qid_type & 0x80 != 0;
                let kind = if is_dir {
                    crate::wasmtime_adapter::wasi::FsNodeKind::Directory
                } else {
                    crate::wasmtime_adapter::wasi::FsNodeKind::File
                };
                if !is_dir {
                    match service.read_file(&host_path).await {
                        Ok(bytes) => (kind, Some(bytes)),
                        Err(err) => {
                            return Ok(Err(error_code_from_p3(map_host_fs_error(err))));
                        }
                    }
                } else {
                    let entries = match service.read_dir(&host_path).await {
                        Ok(e) => e,
                        Err(err) => {
                            return Ok(Err(error_code_from_p3(map_host_fs_error(err))));
                        }
                    };
                    self.filesystem_mut()
                        .seed_host_directory_entries(&absolute, entries);
                    (kind, None)
                }
            } else {
                match service.create_file(&host_path).await {
                    Ok(()) => (crate::wasmtime_adapter::wasi::FsNodeKind::File, Some(Vec::new())),
                    Err(err) => {
                        return Ok(Err(error_code_from_p3(map_host_fs_error(err))));
                    }
                }
            };
            if let Some(bytes) = contents {
                self.filesystem_mut()
                    .seed_host_file_content(&absolute, bytes);
            }
            let opened = crate::wasmtime_adapter::wasi::FsDescriptor {
                path: absolute,
                kind,
                flags: descriptor_flags_p3,
            };
            return Ok(Ok(self.table.push(opened)?));
        }
        let now_nanos = self.now_nanos();
        match self.filesystem_mut().open_at(
            &base,
            path_flags_to_p3(path_flags),
            &path,
            open_flags_p3,
            descriptor_flags_p3,
            now_nanos,
        ) {
            Ok(opened) => Ok(Ok(self.table.push(opened)?)),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn readlink_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<core::result::Result<String, p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    async fn remove_directory_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.remove(&host_path, true).await {
                Ok(()) => {
                    self.filesystem_mut().invalidate_host_subtree(&absolute);
                    Ok(Ok(()))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem_mut().remove_directory_at(&descriptor, &path) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn rename_at(
        &mut self,
        source_descriptor: Resource<FsDescriptor>,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let source_descriptor = match get_fs_descriptor(self, &source_descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let destination_descriptor = match get_fs_descriptor(self, &destination_descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let src_abs =
            match crate::resolve_child_path(&source_descriptor.path, &source_path) {
                Ok(p) => p,
                Err(error) => return Ok(Err(error_code_from_path(error))),
            };
        let dst_abs = match crate::resolve_child_path(
            &destination_descriptor.path,
            &destination_path,
        ) {
            Ok(p) => p,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        let src_host = crate::guest_host_share_path(&src_abs).map(|p| p.to_owned());
        let dst_host = crate::guest_host_share_path(&dst_abs).map(|p| p.to_owned());
        if src_host.is_some() || dst_host.is_some() {
            let Some(src_host) = src_host else {
                return Ok(Err(p2fs::ErrorCode::CrossDevice));
            };
            let Some(dst_host) = dst_host else {
                return Ok(Err(p2fs::ErrorCode::CrossDevice));
            };
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            return match service.rename(&src_host, &dst_host).await {
                Ok(()) => {
                    self.filesystem_mut().invalidate_host_subtree(&src_abs);
                    self.filesystem_mut().invalidate_host_subtree(&dst_abs);
                    Ok(Ok(()))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        let now_nanos = self.now_nanos();
        match self.filesystem_mut().rename_at(
            &source_descriptor,
            &source_path,
            &destination_descriptor,
            &destination_path,
            now_nanos,
        ) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn symlink_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: String,
        _: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    async fn unlink_file_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(p) => p,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.remove(&host_path, false).await {
                Ok(()) => {
                    self.filesystem_mut().invalidate_host_subtree(&absolute);
                    Ok(Ok(()))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem_mut().unlink_file_at(&descriptor, &path) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn is_same_object(
        &mut self,
        a: Resource<FsDescriptor>,
        b: Resource<FsDescriptor>,
    ) -> Result<bool> {
        let left = self.table.get(&a).map_err(wasmtime::Error::from)?.clone();
        let right = self.table.get(&b).map_err(wasmtime::Error::from)?.clone();
        Ok(left.path == right.path && left.kind == right.kind)
    }

    async fn metadata_hash(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::MetadataHashValue, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if let Some(host_path) = crate::guest_host_share_path(&descriptor.path) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.stat_path(&host_path).await {
                Ok(metadata) => Ok(Ok(p2fs::MetadataHashValue {
                    lower: metadata.qid_path,
                    upper: u64::from(metadata.mode) << 32 ^ metadata.size,
                })),
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem_mut().metadata_hash(&descriptor.path) {
            Ok(hash) => Ok(Ok(metadata_hash_from_p3(hash))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn metadata_hash_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
    ) -> Result<core::result::Result<p2fs::MetadataHashValue, p2fs::ErrorCode>> {
        let _ = path_flags;
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.stat_path(&host_path).await {
                Ok(metadata) => Ok(Ok(p2fs::MetadataHashValue {
                    lower: metadata.qid_path,
                    upper: u64::from(metadata.mode) << 32 ^ metadata.size,
                })),
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem_mut().metadata_hash(&absolute) {
            Ok(hash) => Ok(Ok(metadata_hash_from_p3(hash))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn drop(&mut self, resource: Resource<FsDescriptor>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::types::HostDirectoryEntryStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_directory_entry(
        &mut self,
        resource: Resource<DirectoryEntryStream>,
    ) -> Result<core::result::Result<Option<p2fs::DirectoryEntry>, p2fs::ErrorCode>> {
        let stream = self
            .table
            .get_mut(&resource)
            .map_err(wasmtime::Error::from)?;
        let entry = stream.entries.get(stream.cursor).cloned();
        if entry.is_some() {
            stream.cursor += 1;
        }
        Ok(Ok(entry))
    }

    fn drop(&mut self, resource: Resource<DirectoryEntryStream>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> random_bindings::random::random::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0; len as usize])
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl<CpuImpl, HostFs> random_bindings::random::insecure::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0; len as usize])
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl<CpuImpl, HostFs> random_bindings::random::insecure_seed::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }
}

type P2SocketErrorCode = p2net::ErrorCode;
type P2SocketResult<T> = core::result::Result<T, P2SocketErrorCode>;

fn socket_not_supported<T>() -> Result<P2SocketResult<T>> {
    Ok(Err(P2SocketErrorCode::NotSupported))
}

fn socket_unavailable<T>() -> Result<T> {
    Err(wasmtime::Error::msg(
        "sockets are unsupported on the embedded debugger host",
    ))
}

fn delete_resource<R: 'static, CpuImpl, HostFs>(
    store: &mut StoreData<CpuImpl, HostFs>,
    resource: Resource<R>,
) -> Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    store.table.delete(resource)?;
    Ok(())
}

impl<CpuImpl, HostFs> p2net::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn network_error_code(
        &mut self,
        _: Resource<p2io_error::Error>,
    ) -> Result<Option<p2net::ErrorCode>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> p2net::HostNetwork for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<P2Network>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> sockets_bindings::sockets::instance_network::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn instance_network(&mut self) -> Result<Resource<P2Network>> {
        self.table.push(P2Network).map_err(Into::into)
    }
}

impl<CpuImpl, HostFs> p2udp::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> p2udp::HostUdpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn start_bind(
        &mut self,
        _: Resource<UdpSocket>,
        _: Resource<p2udp::Network>,
        _: p2udp::IpSocketAddress,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn finish_bind(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn stream(
        &mut self,
        _: Resource<UdpSocket>,
        _: Option<p2udp::IpSocketAddress>,
    ) -> Result<
        core::result::Result<
            (
                Resource<p2udp::IncomingDatagramStream>,
                Resource<p2udp::OutgoingDatagramStream>,
            ),
            p2udp::ErrorCode,
        >,
    > {
        socket_not_supported()
    }

    fn local_address(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<p2udp::IpSocketAddress, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn remote_address(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<p2udp::IpSocketAddress, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn address_family(&mut self, _: Resource<UdpSocket>) -> Result<p2udp::IpAddressFamily> {
        socket_unavailable()
    }

    fn unicast_hop_limit(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u8, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_unicast_hop_limit(
        &mut self,
        _: Resource<UdpSocket>,
        _: u8,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn receive_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_receive_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn send_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_send_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn subscribe(&mut self, _: Resource<UdpSocket>) -> Result<Resource<p2udp::Pollable>> {
        socket_unavailable()
    }

    fn drop(&mut self, resource: Resource<UdpSocket>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2udp::HostIncomingDatagramStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn receive(
        &mut self,
        _: Resource<p2udp::IncomingDatagramStream>,
        _: u64,
    ) -> Result<core::result::Result<Vec<p2udp::IncomingDatagram>, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn subscribe(
        &mut self,
        _: Resource<p2udp::IncomingDatagramStream>,
    ) -> Result<Resource<p2udp::Pollable>> {
        socket_unavailable()
    }

    fn drop(&mut self, resource: Resource<p2udp::IncomingDatagramStream>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2udp::HostOutgoingDatagramStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn check_send(
        &mut self,
        _: Resource<p2udp::OutgoingDatagramStream>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn send(
        &mut self,
        _: Resource<p2udp::OutgoingDatagramStream>,
        _: Vec<p2udp::OutgoingDatagram>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        socket_not_supported()
    }

    fn subscribe(
        &mut self,
        _: Resource<p2udp::OutgoingDatagramStream>,
    ) -> Result<Resource<p2udp::Pollable>> {
        socket_unavailable()
    }

    fn drop(&mut self, resource: Resource<p2udp::OutgoingDatagramStream>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2udp_create::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn create_udp_socket(
        &mut self,
        _: p2udp_create::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<UdpSocket>, p2udp_create::ErrorCode>> {
        socket_not_supported()
    }
}

impl<CpuImpl, HostFs> p2tcp::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> p2tcp::HostTcpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn start_bind(
        &mut self,
        _: Resource<TcpSocket>,
        _: Resource<p2tcp::Network>,
        _: p2tcp::IpSocketAddress,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn finish_bind(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn start_connect(
        &mut self,
        _: Resource<TcpSocket>,
        _: Resource<p2tcp::Network>,
        _: p2tcp::IpSocketAddress,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn finish_connect(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<
        core::result::Result<
            (Resource<p2tcp::InputStream>, Resource<p2tcp::OutputStream>),
            p2tcp::ErrorCode,
        >,
    > {
        socket_not_supported()
    }

    fn start_listen(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn finish_listen(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn accept(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<
        core::result::Result<
            (
                Resource<TcpSocket>,
                Resource<p2tcp::InputStream>,
                Resource<p2tcp::OutputStream>,
            ),
            p2tcp::ErrorCode,
        >,
    > {
        socket_not_supported()
    }

    fn local_address(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::IpSocketAddress, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn remote_address(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::IpSocketAddress, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn is_listening(&mut self, _: Resource<TcpSocket>) -> Result<bool> {
        socket_unavailable()
    }

    fn address_family(&mut self, _: Resource<TcpSocket>) -> Result<p2tcp::IpAddressFamily> {
        socket_unavailable()
    }

    fn set_listen_backlog_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn keep_alive_enabled(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<bool, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_keep_alive_enabled(
        &mut self,
        _: Resource<TcpSocket>,
        _: bool,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::Duration, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSocket>,
        _: p2tcp::Duration,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn keep_alive_interval(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::Duration, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_keep_alive_interval(
        &mut self,
        _: Resource<TcpSocket>,
        _: p2tcp::Duration,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn keep_alive_count(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u32, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_keep_alive_count(
        &mut self,
        _: Resource<TcpSocket>,
        _: u32,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn hop_limit(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u8, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_hop_limit(
        &mut self,
        _: Resource<TcpSocket>,
        _: u8,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn receive_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_receive_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn send_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn set_send_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn subscribe(&mut self, _: Resource<TcpSocket>) -> Result<Resource<p2tcp::Pollable>> {
        socket_unavailable()
    }

    fn shutdown(
        &mut self,
        _: Resource<TcpSocket>,
        _: p2tcp::ShutdownType,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn drop(&mut self, resource: Resource<TcpSocket>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2tcp_create::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn create_tcp_socket(
        &mut self,
        _: p2tcp_create::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<TcpSocket>, p2tcp_create::ErrorCode>> {
        socket_not_supported()
    }
}

impl<CpuImpl, HostFs> p2lookup::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn resolve_addresses(
        &mut self,
        _: Resource<p2lookup::Network>,
        _: String,
    ) -> Result<core::result::Result<Resource<p2lookup::ResolveAddressStream>, p2lookup::ErrorCode>>
    {
        socket_not_supported()
    }
}

impl<CpuImpl, HostFs> p2lookup::HostResolveAddressStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn resolve_next_address(
        &mut self,
        _: Resource<p2lookup::ResolveAddressStream>,
    ) -> Result<core::result::Result<Option<p2lookup::IpAddress>, p2lookup::ErrorCode>> {
        socket_not_supported()
    }

    fn subscribe(
        &mut self,
        _: Resource<p2lookup::ResolveAddressStream>,
    ) -> Result<Resource<p2lookup::Pollable>> {
        socket_unavailable()
    }

    fn drop(&mut self, resource: Resource<p2lookup::ResolveAddressStream>) -> Result<()> {
        delete_resource(self, resource)
    }
}

fn system_time_from_nanos(nanos: u64) -> clocks_bindings::clocks::wall_clock::Datetime {
    clocks_bindings::clocks::wall_clock::Datetime {
        seconds: nanos / 1_000_000_000,
        nanoseconds: (nanos % 1_000_000_000) as u32,
    }
}

fn get_fs_descriptor<CpuImpl, HostFs>(
    store: &mut StoreData<CpuImpl, HostFs>,
    resource: &Resource<FsDescriptor>,
) -> core::result::Result<FsDescriptor, p2fs::ErrorCode>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    store
        .table
        .get(resource)
        .cloned()
        .map_err(super::fs_resource_error)
        .map_err(error_code_from_p3)
}

fn descriptor_type_from_kind(kind: FsNodeKind) -> p2fs::DescriptorType {
    match kind {
        FsNodeKind::Directory => p2fs::DescriptorType::Directory,
        FsNodeKind::File => p2fs::DescriptorType::RegularFile,
    }
}

fn descriptor_type_from_p3(kind: p3fs::DescriptorType) -> p2fs::DescriptorType {
    match kind {
        p3fs::DescriptorType::BlockDevice => p2fs::DescriptorType::BlockDevice,
        p3fs::DescriptorType::CharacterDevice => p2fs::DescriptorType::CharacterDevice,
        p3fs::DescriptorType::Directory => p2fs::DescriptorType::Directory,
        p3fs::DescriptorType::Fifo => p2fs::DescriptorType::Fifo,
        p3fs::DescriptorType::SymbolicLink => p2fs::DescriptorType::SymbolicLink,
        p3fs::DescriptorType::RegularFile => p2fs::DescriptorType::RegularFile,
        p3fs::DescriptorType::Socket => p2fs::DescriptorType::Socket,
        p3fs::DescriptorType::Other(_) => p2fs::DescriptorType::Unknown,
    }
}

fn descriptor_flags_from_p3(flags: p3fs::DescriptorFlags) -> p2fs::DescriptorFlags {
    let mut result = p2fs::DescriptorFlags::empty();
    if flags.contains(p3fs::DescriptorFlags::READ) {
        result |= p2fs::DescriptorFlags::READ;
    }
    if flags.contains(p3fs::DescriptorFlags::WRITE) {
        result |= p2fs::DescriptorFlags::WRITE;
    }
    if flags.contains(p3fs::DescriptorFlags::FILE_INTEGRITY_SYNC) {
        result |= p2fs::DescriptorFlags::FILE_INTEGRITY_SYNC;
    }
    if flags.contains(p3fs::DescriptorFlags::DATA_INTEGRITY_SYNC) {
        result |= p2fs::DescriptorFlags::DATA_INTEGRITY_SYNC;
    }
    if flags.contains(p3fs::DescriptorFlags::REQUESTED_WRITE_SYNC) {
        result |= p2fs::DescriptorFlags::REQUESTED_WRITE_SYNC;
    }
    if flags.contains(p3fs::DescriptorFlags::MUTATE_DIRECTORY) {
        result |= p2fs::DescriptorFlags::MUTATE_DIRECTORY;
    }
    result
}

fn descriptor_flags_to_p3(flags: p2fs::DescriptorFlags) -> p3fs::DescriptorFlags {
    let mut result = p3fs::DescriptorFlags::empty();
    if flags.contains(p2fs::DescriptorFlags::READ) {
        result |= p3fs::DescriptorFlags::READ;
    }
    if flags.contains(p2fs::DescriptorFlags::WRITE) {
        result |= p3fs::DescriptorFlags::WRITE;
    }
    if flags.contains(p2fs::DescriptorFlags::FILE_INTEGRITY_SYNC) {
        result |= p3fs::DescriptorFlags::FILE_INTEGRITY_SYNC;
    }
    if flags.contains(p2fs::DescriptorFlags::DATA_INTEGRITY_SYNC) {
        result |= p3fs::DescriptorFlags::DATA_INTEGRITY_SYNC;
    }
    if flags.contains(p2fs::DescriptorFlags::REQUESTED_WRITE_SYNC) {
        result |= p3fs::DescriptorFlags::REQUESTED_WRITE_SYNC;
    }
    if flags.contains(p2fs::DescriptorFlags::MUTATE_DIRECTORY) {
        result |= p3fs::DescriptorFlags::MUTATE_DIRECTORY;
    }
    result
}

fn open_flags_to_p3(flags: p2fs::OpenFlags) -> p3fs::OpenFlags {
    let mut result = p3fs::OpenFlags::empty();
    if flags.contains(p2fs::OpenFlags::CREATE) {
        result |= p3fs::OpenFlags::CREATE;
    }
    if flags.contains(p2fs::OpenFlags::DIRECTORY) {
        result |= p3fs::OpenFlags::DIRECTORY;
    }
    if flags.contains(p2fs::OpenFlags::EXCLUSIVE) {
        result |= p3fs::OpenFlags::EXCLUSIVE;
    }
    if flags.contains(p2fs::OpenFlags::TRUNCATE) {
        result |= p3fs::OpenFlags::TRUNCATE;
    }
    result
}

fn path_flags_to_p3(flags: p2fs::PathFlags) -> p3fs::PathFlags {
    let mut result = p3fs::PathFlags::empty();
    if flags.contains(p2fs::PathFlags::SYMLINK_FOLLOW) {
        result |= p3fs::PathFlags::SYMLINK_FOLLOW;
    }
    result
}

fn descriptor_stat_from_p3(stat: p3fs::DescriptorStat) -> p2fs::DescriptorStat {
    p2fs::DescriptorStat {
        type_: descriptor_type_from_p3(stat.type_),
        link_count: stat.link_count,
        size: stat.size,
        data_access_timestamp: stat.data_access_timestamp.map(datetime_from_p3),
        data_modification_timestamp: stat.data_modification_timestamp.map(datetime_from_p3),
        status_change_timestamp: stat.status_change_timestamp.map(datetime_from_p3),
    }
}

fn metadata_hash_from_p3(hash: p3fs::MetadataHashValue) -> p2fs::MetadataHashValue {
    p2fs::MetadataHashValue {
        lower: hash.lower,
        upper: hash.upper,
    }
}

fn directory_entry_from_p3(entry: p3fs::DirectoryEntry) -> p2fs::DirectoryEntry {
    p2fs::DirectoryEntry {
        type_: descriptor_type_from_p3(entry.type_),
        name: entry.name,
    }
}

fn datetime_from_p3(
    instant: crate::wasmtime_adapter::wasi::bindings::clocks::system_clock::Instant,
) -> p2fs::Datetime {
    p2fs::Datetime {
        seconds: instant
            .seconds
            .try_into()
            .expect("preview2 filesystem cannot represent a negative timestamp"),
        nanoseconds: instant.nanoseconds,
    }
}

fn error_code_from_path(error: crate::ComponentFsPathError) -> p2fs::ErrorCode {
    match error {
        crate::ComponentFsPathError::InvalidBasePath => p2fs::ErrorCode::Invalid,
        crate::ComponentFsPathError::NotPermitted => p2fs::ErrorCode::NotPermitted,
    }
}

fn error_code_from_p3(error: p3fs::ErrorCode) -> p2fs::ErrorCode {
    match error {
        p3fs::ErrorCode::Access => p2fs::ErrorCode::Access,
        p3fs::ErrorCode::Already => p2fs::ErrorCode::Already,
        p3fs::ErrorCode::BadDescriptor => p2fs::ErrorCode::BadDescriptor,
        p3fs::ErrorCode::Busy => p2fs::ErrorCode::Busy,
        p3fs::ErrorCode::Deadlock => p2fs::ErrorCode::Deadlock,
        p3fs::ErrorCode::Quota => p2fs::ErrorCode::Quota,
        p3fs::ErrorCode::Exist => p2fs::ErrorCode::Exist,
        p3fs::ErrorCode::FileTooLarge => p2fs::ErrorCode::FileTooLarge,
        p3fs::ErrorCode::IllegalByteSequence => p2fs::ErrorCode::IllegalByteSequence,
        p3fs::ErrorCode::InProgress => p2fs::ErrorCode::InProgress,
        p3fs::ErrorCode::Interrupted => p2fs::ErrorCode::Interrupted,
        p3fs::ErrorCode::Invalid => p2fs::ErrorCode::Invalid,
        p3fs::ErrorCode::Io => p2fs::ErrorCode::Io,
        p3fs::ErrorCode::IsDirectory => p2fs::ErrorCode::IsDirectory,
        p3fs::ErrorCode::Loop => p2fs::ErrorCode::Loop,
        p3fs::ErrorCode::TooManyLinks => p2fs::ErrorCode::TooManyLinks,
        p3fs::ErrorCode::MessageSize => p2fs::ErrorCode::MessageSize,
        p3fs::ErrorCode::NameTooLong => p2fs::ErrorCode::NameTooLong,
        p3fs::ErrorCode::NoDevice => p2fs::ErrorCode::NoDevice,
        p3fs::ErrorCode::NoEntry => p2fs::ErrorCode::NoEntry,
        p3fs::ErrorCode::NoLock => p2fs::ErrorCode::NoLock,
        p3fs::ErrorCode::InsufficientMemory => p2fs::ErrorCode::InsufficientMemory,
        p3fs::ErrorCode::InsufficientSpace => p2fs::ErrorCode::InsufficientSpace,
        p3fs::ErrorCode::NotDirectory => p2fs::ErrorCode::NotDirectory,
        p3fs::ErrorCode::NotEmpty => p2fs::ErrorCode::NotEmpty,
        p3fs::ErrorCode::NotRecoverable => p2fs::ErrorCode::NotRecoverable,
        p3fs::ErrorCode::Unsupported => p2fs::ErrorCode::Unsupported,
        p3fs::ErrorCode::NoTty => p2fs::ErrorCode::NoTty,
        p3fs::ErrorCode::NoSuchDevice => p2fs::ErrorCode::NoSuchDevice,
        p3fs::ErrorCode::Overflow => p2fs::ErrorCode::Overflow,
        p3fs::ErrorCode::NotPermitted => p2fs::ErrorCode::NotPermitted,
        p3fs::ErrorCode::Pipe => p2fs::ErrorCode::Pipe,
        p3fs::ErrorCode::ReadOnly => p2fs::ErrorCode::ReadOnly,
        p3fs::ErrorCode::InvalidSeek => p2fs::ErrorCode::InvalidSeek,
        p3fs::ErrorCode::TextFileBusy => p2fs::ErrorCode::TextFileBusy,
        p3fs::ErrorCode::CrossDevice => p2fs::ErrorCode::CrossDevice,
        p3fs::ErrorCode::Other(_) => {
            panic!("preview2 filesystem cannot represent preview3 error-code::other")
        }
    }
}
