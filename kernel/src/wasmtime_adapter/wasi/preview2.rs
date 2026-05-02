use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use thiserror::Error;
use wasmtime::component::{HasSelf, Linker, Resource};
use wasmtime::{self, Result};
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::{DynPollable, Pollable, subscribe};
use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, Error as IoError, InputStream, OutputStream, StreamError,
};
use wasmtime_wasi_io::{self};

use super::bindings::clocks::system_clock::Instant as P3SystemInstant;
use super::bindings::filesystem::types as p3fs;
use super::bindings::filesystem::types::{ErrorCode as P3ErrorCode, OpenFlags as P3OpenFlags};
use super::{
    DebugFileSystem, FsDescriptor, FsNodeKind, P2IncomingDatagramStream, P2Network,
    P2OutgoingDatagramStream, P2ResolveAddressStream, Preview2GuestExit, TcpSocket, UdpSocket,
    WasiAdapterTrap, WasiImportSet, WasiTcpSocketAddress, WasiTcpSocketFamily,
    WasiUdpSocketAddress, WasiUdpSocketError, WasiUdpSocketFamily, has_wasi_network_rights,
    metadata_hash_value, wasi_udp_bind_rights,
};
use crate::wasmtime_adapter::component_host::{
    HostRuntimeState, RuntimeDeadlinePollable, StoreData,
};
use crate::wasmtime_adapter::store::{ChannelInputStream, ChannelOutputStream, StdioOutputStream};
use crate::wasmtime_adapter::wasi::map_host_fs_error;
use crate::{ComponentNetworkService, ComponentOutputMode, ComponentOutputStreamKind};

#[cfg(test)]
pub(crate) const PREVIEW2_LINKED_INTERFACES: &[&str] = &[
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
    "wasi:clocks/monotonic-clock",
    "wasi:clocks/system-clock",
    "wasi:filesystem/preopens",
    "wasi:filesystem/types",
    "wasi:random/random",
    "wasi:random/insecure",
    "wasi:random/insecure-seed",
    "wasi:sockets/network",
    "wasi:sockets/instance-network",
    "wasi:sockets/udp",
    "wasi:sockets/udp-create-socket",
    "wasi:sockets/tcp",
    "wasi:sockets/tcp-create-socket",
    "wasi:sockets/ip-name-lookup",
];

#[cfg(test)]
pub(crate) const PREVIEW2_WIT_PACKAGES: &[(&str, &str)] = &[
    (
        "wasi:cli",
        include_str!("../../../../../wasmtime/crates/wasi/src/p2/wit/deps/cli.wit"),
    ),
    (
        "wasi:clocks",
        include_str!("../../../../../wasmtime/crates/wasi/src/p2/wit/deps/clocks.wit"),
    ),
    (
        "wasi:filesystem",
        include_str!("../../../../../wasmtime/crates/wasi/src/p2/wit/deps/filesystem.wit"),
    ),
    (
        "wasi:random",
        include_str!("../../../../../wasmtime/crates/wasi/src/p2/wit/deps/random.wit"),
    ),
    (
        "wasi:sockets",
        include_str!("../../../../../wasmtime/crates/wasi/src/p2/wit/deps/sockets.wit"),
    ),
];

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
                "wasi:cli/terminal-input.terminal-input": crate::wasmtime_adapter::wasi::preview2::TerminalInput,
                "wasi:cli/terminal-output.terminal-output": crate::wasmtime_adapter::wasi::preview2::TerminalOutput,
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
                "wasi:filesystem/types.[method]descriptor.read": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.write": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.read-directory": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.set-size": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.create-directory-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.link-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.readlink-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.unlink-file-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.rename-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.remove-directory-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.symlink-at": async | tracing | trappable,
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
                "wasi:filesystem/types.directory-entry-stream": crate::wasmtime_adapter::wasi::preview2::DirectoryEntryStream,
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
            imports: {
                "wasi:sockets/udp.[method]udp-socket.start-bind": async | tracing | trappable,
                "wasi:sockets/udp.[method]udp-socket.stream": async | tracing | trappable,
                "wasi:sockets/udp.[method]incoming-datagram-stream.receive": async | tracing | trappable,
                "wasi:sockets/udp.[method]outgoing-datagram-stream.send": async | tracing | trappable,
                default: tracing | trappable,
            },
            with: {
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
                "wasi:io/streams.input-stream": wasmtime_wasi_io::streams::DynInputStream,
                "wasi:io/streams.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
                "wasi:sockets/network.network": crate::wasmtime_adapter::wasi::P2Network,
                "wasi:sockets/tcp.tcp-socket": crate::wasmtime_adapter::wasi::TcpSocket,
                "wasi:sockets/udp.udp-socket": crate::wasmtime_adapter::wasi::UdpSocket,
                "wasi:sockets/udp.incoming-datagram-stream": crate::wasmtime_adapter::wasi::P2IncomingDatagramStream,
                "wasi:sockets/udp.outgoing-datagram-stream": crate::wasmtime_adapter::wasi::P2OutgoingDatagramStream,
                "wasi:sockets/ip-name-lookup.resolve-address-stream": crate::wasmtime_adapter::wasi::P2ResolveAddressStream,
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
    bytes: Bytes,
    cursor: usize,
}

struct FileOutputStream<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    cpu: CpuImpl,
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    filesystem: DebugFileSystem<HostRuntimeState<CpuImpl, HostFs>, HostFs>,
    descriptor: FsDescriptor,
    offset: u64,
    append: bool,
}

#[derive(Debug, Error)]
enum P2FileStreamError {
    #[error("preview2 file stream write failed: {0:?}")]
    WriteFailed(p2fs::ErrorCode),
}

impl FileInputStream {
    fn new(bytes: Bytes) -> Self {
        Self { bytes, cursor: 0 }
    }
}

impl<CpuImpl, HostFs> FileOutputStream<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        cpu: CpuImpl,
        runtime_state: HostRuntimeState<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        offset: u64,
        append: bool,
    ) -> Self {
        Self {
            cpu,
            runtime_state: runtime_state.clone(),
            filesystem: DebugFileSystem::new(runtime_state),
            descriptor,
            offset,
            append,
        }
    }

    fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }
}

pub(crate) fn add_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
    imports: &WasiImportSet,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if imports.has("wasi:cli/environment", "0.2") {
        cli_bindings::cli::environment::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/exit", "0.2") {
        cli_bindings::cli::exit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            &Default::default(),
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdin", "0.2") {
        cli_bindings::cli::stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdout", "0.2") {
        cli_bindings::cli::stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stderr", "0.2") {
        cli_bindings::cli::stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-input", "0.2") {
        cli_bindings::cli::terminal_input::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-output", "0.2") {
        cli_bindings::cli::terminal_output::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stdin", "0.2") {
        cli_bindings::cli::terminal_stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stdout", "0.2") {
        cli_bindings::cli::terminal_stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stderr", "0.2") {
        cli_bindings::cli::terminal_stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }

    if imports.has("wasi:clocks/monotonic-clock", "0.2") {
        clocks_bindings::clocks::monotonic_clock::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:clocks/wall-clock", "0.2") {
        clocks_bindings::clocks::wall_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }

    if imports.has("wasi:filesystem/preopens", "0.2") {
        filesystem_bindings::filesystem::preopens::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:filesystem/types", "0.2") {
        filesystem_bindings::filesystem::types::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }

    if imports.has("wasi:random/random", "0.2") {
        random_bindings::random::random::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/insecure", "0.2") {
        random_bindings::random::insecure::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/insecure-seed", "0.2") {
        random_bindings::random::insecure_seed::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/network", "0.2") {
        sockets_bindings::sockets::network::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            &Default::default(),
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/instance-network", "0.2") {
        sockets_bindings::sockets::instance_network::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/udp", "0.2") {
        sockets_bindings::sockets::udp::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/udp-create-socket", "0.2") {
        sockets_bindings::sockets::udp_create_socket::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/tcp", "0.2") {
        sockets_bindings::sockets::tcp::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/tcp-create-socket", "0.2") {
        sockets_bindings::sockets::tcp_create_socket::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/ip-name-lookup", "0.2") {
        sockets_bindings::sockets::ip_name_lookup::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, HostFs>>,
        >(linker, |state| state)?;
    }
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
        let chunk = self.bytes.slice(self.cursor..end);
        self.cursor = end;
        Ok(chunk)
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, HostFs> Pollable for FileOutputStream<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, HostFs> OutputStream for FileOutputStream<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn write(&mut self, bytes: Bytes) -> core::result::Result<(), StreamError> {
        let now_nanos = self.now_nanos();
        if self.append {
            self.filesystem
                .append(&self.descriptor, bytes.as_ref(), now_nanos)
                .map_err(|error| p2_stream_error(error_code_from_p3(error)))?;
        } else {
            let offset = usize::try_from(self.offset)
                .map_err(|_| p2_stream_error(p2fs::ErrorCode::Overflow))?;
            self.filesystem
                .write_at(&self.descriptor, offset, bytes.as_ref(), now_nanos)
                .map_err(|error| p2_stream_error(error_code_from_p3(error)))?;
            self.offset = self.offset.saturating_add(bytes.len() as u64);
        }
        Ok(())
    }

    fn flush(&mut self) -> core::result::Result<(), StreamError> {
        Ok(())
    }

    fn check_write(&mut self) -> core::result::Result<usize, StreamError> {
        if !self.descriptor.flags.contains(p3fs::DescriptorFlags::WRITE) {
            return Err(p2_stream_error(p2fs::ErrorCode::NotPermitted));
        }
        Ok(64 * 1024)
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for TcpSocket {
    async fn ready(&mut self) {
        loop {
            if !self.inner.lock().connect_in_progress {
                return;
            }
            self.ready.notified().await;
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for UdpSocket {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for P2IncomingDatagramStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for P2OutgoingDatagramStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for P2ResolveAddressStream {
    async fn ready(&mut self) {
        self.wait_ready().await;
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::environment::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment().to_vec())
    }

    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments().to_vec())
    }

    fn initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(self
            .process_authority()
            .cwd()
            .map(|cwd| cwd.guest_name().to_owned()))
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::exit::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let code = match status {
            Ok(()) => 0,
            Err(()) => 1,
        };
        self.request_exit(code);
        Err(wasmtime::Error::new(Preview2GuestExit::new(u32::from(
            code,
        ))))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        self.request_exit(status_code);
        Err(wasmtime::Error::new(Preview2GuestExit::new(u32::from(
            status_code,
        ))))
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_stdout(&mut self) -> Result<Resource<DynOutputStream>> {
        let stream = build_stdio_stream(self, ComponentOutputStreamKind::Stdout);
        Ok(self.table.push(Box::new(stream) as DynOutputStream)?)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
    CpuImpl: Cpu + Clone,
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
        ComponentOutputMode::Serial => StdioOutputStream::Serial(store.serial_writer_fn()),
        ComponentOutputMode::Trace => StdioOutputStream::Trace,
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_input::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> cli_bindings::cli::terminal_output::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_input::HostTerminalInput
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_output::HostTerminalOutput
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> cli_bindings::cli::terminal_stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> clocks_bindings::clocks::monotonic_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
            self.timer(),
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
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<clocks_bindings::clocks::wall_clock::Datetime> {
        Ok(system_time_from_nanos(self.system_time_nanos()))
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
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_directories(&mut self) -> Result<Vec<(Resource<FsDescriptor>, String)>> {
        let preopens = self.process_authority().directory_preopens().to_vec();
        preopens
            .iter()
            .map(|preopen| {
                let descriptor = super::preopen_descriptor(preopen);
                let resource = self.table.push(descriptor)?;
                Ok((resource, preopen.guest_name().to_owned()))
            })
            .collect()
    }
}

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn filesystem_error_code(&mut self, _: Resource<IoError>) -> Result<Option<p2fs::ErrorCode>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::types::HostDescriptor
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if !descriptor.flags.contains(p3fs::DescriptorFlags::WRITE) {
            return Ok(Err(p2fs::ErrorCode::NotPermitted));
        }
        let stream = FileOutputStream::new(
            self.cpu.clone(),
            self.runtime_state.clone(),
            descriptor,
            offset,
            false,
        );
        Ok(Ok(self.table.push(Box::new(stream) as DynOutputStream)?))
    }

    fn append_via_stream(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if !descriptor.flags.contains(p3fs::DescriptorFlags::WRITE) {
            return Ok(Err(p2fs::ErrorCode::NotPermitted));
        }
        let stream = FileOutputStream::new(
            self.cpu.clone(),
            self.runtime_state.clone(),
            descriptor,
            0,
            true,
        );
        Ok(Ok(self.table.push(Box::new(stream) as DynOutputStream)?))
    }

    fn advise(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: p2fs::Advice,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        match get_fs_descriptor(self, &descriptor) {
            Ok(_) => Ok(Ok(())),
            Err(error) => Ok(Err(error)),
        }
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

    async fn set_size(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        size: u64,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if let Some(host_path) = crate::guest_host_share_path(&descriptor.path) {
            if !descriptor.flags.contains(p3fs::DescriptorFlags::WRITE) {
                return Ok(Err(p2fs::ErrorCode::NotPermitted));
            }
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.set_file_size(&host_path, size).await {
                Ok(()) => {
                    self.filesystem_mut()
                        .invalidate_host_subtree(&descriptor.path);
                    Ok(Ok(()))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        let now_nanos = self.now_nanos();
        match self.filesystem_mut().set_size(&descriptor, size, now_nanos) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn set_times(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        data_access_timestamp: p2fs::NewTimestamp,
        data_modification_timestamp: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let now_nanos = self.system_time_nanos();
        let access_nanos = match p2_new_timestamp_nanos(data_access_timestamp, now_nanos) {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        let modified_nanos = match p2_new_timestamp_nanos(data_modification_timestamp, now_nanos) {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self
            .filesystem_mut()
            .set_times(&descriptor, access_nanos, modified_nanos, now_nanos)
        {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn read(
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
        if let Some(host_path) = crate::guest_host_share_path(&descriptor.path) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let max_bytes = match u32::try_from(length) {
                Ok(max_bytes) => max_bytes,
                Err(_) => return Ok(Err(p2fs::ErrorCode::Overflow)),
            };
            let host_path = host_path.to_owned();
            return match service.read_file_range(&host_path, offset, max_bytes).await {
                Ok(bytes) => {
                    let eof = bytes.len() < length;
                    Ok(Ok((bytes, eof)))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self
            .filesystem_mut()
            .read_file_chunk(&descriptor, offset, length)
        {
            Ok(bytes) => {
                let eof = bytes.len() < length;
                Ok(Ok((bytes.to_vec(), eof)))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn write(
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
        if let Some(host_path) = crate::guest_host_share_path(&descriptor.path) {
            if !descriptor.flags.contains(p3fs::DescriptorFlags::WRITE) {
                return Ok(Err(p2fs::ErrorCode::NotPermitted));
            }
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.write_file(&host_path, offset as u64, &buffer).await {
                Ok(()) => {
                    self.filesystem_mut()
                        .invalidate_host_subtree(&descriptor.path);
                    Ok(Ok(buffer.len() as u64))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
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
            if let Ok(service) = self.filesystem().host_service().map_err(error_code_from_p3) {
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
        descriptor: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        path: String,
        data_access_timestamp: p2fs::NewTimestamp,
        data_modification_timestamp: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let now_nanos = self.system_time_nanos();
        let access_nanos = match p2_new_timestamp_nanos(data_access_timestamp, now_nanos) {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        let modified_nanos = match p2_new_timestamp_nanos(data_modification_timestamp, now_nanos) {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        match self.filesystem_mut().set_times_at_path(
            &absolute,
            access_nanos,
            modified_nanos,
            now_nanos,
        ) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    async fn link_at(
        &mut self,
        source_descriptor: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        if self.process_authority().derive_link_source_cap().is_err()
            || self
                .process_authority()
                .derive_link_target_directory_cap()
                .is_err()
        {
            return Ok(Err(p2fs::ErrorCode::NotPermitted));
        }
        let source_descriptor = match get_fs_descriptor(self, &source_descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let destination_descriptor = match get_fs_descriptor(self, &destination_descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let source_absolute = match crate::resolve_child_path(&source_descriptor.path, &source_path)
        {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        let destination_absolute =
            match crate::resolve_child_path(&destination_descriptor.path, &destination_path) {
                Ok(path) => path,
                Err(error) => return Ok(Err(error_code_from_path(error))),
            };
        let source_host =
            crate::guest_host_share_path(&source_absolute).map(|path| path.to_owned());
        let destination_host =
            crate::guest_host_share_path(&destination_absolute).map(|path| path.to_owned());
        if source_host.is_some() || destination_host.is_some() {
            if source_descriptor.kind != FsNodeKind::Directory
                || destination_descriptor.kind != FsNodeKind::Directory
            {
                return Ok(Err(p2fs::ErrorCode::NotDirectory));
            }
            if !destination_descriptor
                .flags
                .contains(p3fs::DescriptorFlags::MUTATE_DIRECTORY)
            {
                return Ok(Err(p2fs::ErrorCode::ReadOnly));
            }
            let Some(source_host) = source_host else {
                return Ok(Err(p2fs::ErrorCode::CrossDevice));
            };
            let Some(destination_host) = destination_host else {
                return Ok(Err(p2fs::ErrorCode::CrossDevice));
            };
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            return match service.hard_link(&source_host, &destination_host).await {
                Ok(()) => {
                    self.filesystem_mut()
                        .invalidate_host_subtree(&destination_absolute);
                    Ok(Ok(()))
                }
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        let now_nanos = self.now_nanos();
        match self.filesystem_mut().link_at(
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

    async fn open_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
        open_flags: p2fs::OpenFlags,
        flags: p2fs::DescriptorFlags,
    ) -> Result<core::result::Result<Resource<FsDescriptor>, p2fs::ErrorCode>> {
        let _ = path_flags;
        let base = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        if base.kind != crate::wasmtime_adapter::wasi::FsNodeKind::Directory {
            return Ok(Err(p2fs::ErrorCode::NotDirectory));
        }
        let open_flags_p3 = open_flags_to_p3(open_flags);
        let descriptor_flags_p3 = descriptor_flags_to_p3(flags);
        if let Err(error) =
            super::validate_descriptor_flags_within_base(base.flags, descriptor_flags_p3)
        {
            return Ok(Err(error_code_from_p3(error)));
        }
        let absolute = match crate::resolve_child_path(&base.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            let metadata = match service.stat_path(&host_path).await {
                Ok(meta) => Some(meta),
                Err(err) => {
                    let code = map_host_fs_error(err);
                    if matches!(code, P3ErrorCode::NoEntry)
                        && open_flags_p3.contains(P3OpenFlags::CREATE)
                    {
                        None
                    } else {
                        return Ok(Err(error_code_from_p3(code)));
                    }
                }
            };
            let (kind, identity, contents) = if let Some(meta) = metadata {
                let is_dir = meta.qid_type & 0x80 != 0;
                let kind = if is_dir {
                    crate::wasmtime_adapter::wasi::FsNodeKind::Directory
                } else {
                    crate::wasmtime_adapter::wasi::FsNodeKind::File
                };
                if !is_dir {
                    match service.read_file(&host_path).await {
                        Ok(bytes) => (kind, meta.identity, Some(bytes)),
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
                    (kind, meta.identity, None)
                }
            } else {
                match service.create_file(&host_path).await {
                    Ok(()) => match service.stat_path(&host_path).await {
                        Ok(metadata) => (
                            crate::wasmtime_adapter::wasi::FsNodeKind::File,
                            metadata.identity,
                            Some(Vec::new()),
                        ),
                        Err(err) => {
                            return Ok(Err(error_code_from_p3(map_host_fs_error(err))));
                        }
                    },
                    Err(err) => {
                        return Ok(Err(error_code_from_p3(map_host_fs_error(err))));
                    }
                }
            };
            if let Some(bytes) = contents {
                self.filesystem_mut()
                    .seed_host_file_content(&absolute, identity, bytes);
            }
            let opened = crate::wasmtime_adapter::wasi::FsDescriptor {
                path: absolute,
                kind,
                flags: descriptor_flags_p3,
                identity: Some(identity),
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

    async fn readlink_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<String, p2fs::ErrorCode>> {
        if self.process_authority().derive_symlink_read_cap().is_err() {
            return Ok(Err(p2fs::ErrorCode::NotPermitted));
        }
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            if descriptor.kind != FsNodeKind::Directory {
                return Ok(Err(p2fs::ErrorCode::NotDirectory));
            }
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.read_link(&host_path).await {
                Ok(payload) => match super::resolve_symlink_payload(&absolute, &payload) {
                    Ok(_) => Ok(Ok(payload)),
                    Err(error) => Ok(Err(error_code_from_p3(error))),
                },
                Err(err) => Ok(Err(error_code_from_p3(map_host_fs_error(err)))),
            };
        }
        match self.filesystem().readlink_at(&descriptor, &path) {
            Ok(payload) => Ok(Ok(payload)),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
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
        match self
            .filesystem_mut()
            .remove_directory_at(&descriptor, &path)
        {
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
        let src_abs = match crate::resolve_child_path(&source_descriptor.path, &source_path) {
            Ok(p) => p,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        let dst_abs =
            match crate::resolve_child_path(&destination_descriptor.path, &destination_path) {
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

    async fn symlink_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        old_path: String,
        new_path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        if self
            .process_authority()
            .derive_symlink_create_cap()
            .is_err()
        {
            return Ok(Err(p2fs::ErrorCode::NotPermitted));
        }
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let absolute = match crate::resolve_child_path(&descriptor.path, &new_path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        if descriptor.kind != FsNodeKind::Directory {
            return Ok(Err(p2fs::ErrorCode::NotDirectory));
        }
        if let Err(error) = super::resolve_symlink_payload(&absolute, &old_path) {
            return Ok(Err(error_code_from_p3(error)));
        }
        if let Some(host_path) = crate::guest_host_share_path(&absolute) {
            if !descriptor
                .flags
                .contains(p3fs::DescriptorFlags::MUTATE_DIRECTORY)
            {
                return Ok(Err(p2fs::ErrorCode::ReadOnly));
            }
            let service = match self.filesystem().host_service() {
                Ok(service) => service,
                Err(error) => return Ok(Err(error_code_from_p3(error))),
            };
            let host_path = host_path.to_owned();
            return match service.symlink(&old_path, &host_path).await {
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
            .symlink_at(&descriptor, &new_path, &old_path, now_nanos)
        {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
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
        Ok(left
            .identity
            .zip(right.identity)
            .is_some_and(|(left, right)| left == right))
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
                Ok(metadata) => Ok(Ok(metadata_hash_from_p3(metadata_hash_value(
                    metadata.identity,
                    u64::from(metadata.mode) << 32 ^ metadata.size,
                )))),
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
                Ok(metadata) => Ok(Ok(metadata_hash_from_p3(metadata_hash_value(
                    metadata.identity,
                    u64::from(metadata.mode) << 32 ^ metadata.size,
                )))),
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

impl<CpuImpl, HostFs> filesystem_bindings::filesystem::types::HostDirectoryEntryStream
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        let len = super::random_len(len)?;
        let mut bytes = vec![0_u8; len];
        self.fill_secure_random(&mut bytes)
            .map_err(super::entropy_error)?;
        Ok(bytes)
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        self.secure_random_u64().map_err(super::entropy_error)
    }
}

impl<CpuImpl, HostFs> random_bindings::random::insecure::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(self.insecure_random_bytes(super::random_len(len)?))
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(self.insecure_random_u64())
    }
}

impl<CpuImpl, HostFs> random_bindings::random::insecure_seed::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok(self.insecure_seed())
    }
}

type P2SocketErrorCode = p2net::ErrorCode;
type P2SocketResult<T> = core::result::Result<T, P2SocketErrorCode>;

fn socket_not_supported<T>() -> Result<P2SocketResult<T>> {
    Ok(Err(P2SocketErrorCode::NotSupported))
}

fn delete_resource<R: 'static, CpuImpl, HostFs>(
    store: &mut StoreData<CpuImpl, HostFs>,
    resource: Resource<R>,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store.table.delete(resource)?;
    Ok(())
}

impl<CpuImpl, HostFs> p2net::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<P2Network>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> sockets_bindings::sockets::instance_network::Host
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn instance_network(&mut self) -> Result<Resource<P2Network>> {
        self.table.push(P2Network).map_err(Into::into)
    }
}

impl<CpuImpl, HostFs> p2udp::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> p2udp::HostUdpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn start_bind(
        &mut self,
        socket: Resource<UdpSocket>,
        network: Resource<p2udp::Network>,
        local_address: p2udp::IpSocketAddress,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::UDP) {
            return Ok(Err(p2udp::ErrorCode::AccessDenied));
        }
        let _ = self.table.get(&network)?;
        let socket = self.table.get(&socket)?.clone();
        let local_address = match parse_p2_udp_socket_address(local_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(map_p2_udp_error(error))),
        };
        if !has_wasi_network_rights(
            self.process_authority(),
            wasi_udp_bind_rights(local_address.port),
        ) {
            return Ok(Err(p2udp::ErrorCode::AccessDenied));
        }
        Ok(socket
            .start_bind_p2(local_address)
            .await
            .map_err(map_p2_udp_error))
    }

    fn finish_bind(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.finish_bind_p2().map_err(map_p2_udp_error))
    }

    async fn stream(
        &mut self,
        socket_resource: Resource<UdpSocket>,
        remote_address: Option<p2udp::IpSocketAddress>,
    ) -> Result<
        core::result::Result<
            (
                Resource<p2udp::IncomingDatagramStream>,
                Resource<p2udp::OutgoingDatagramStream>,
            ),
            p2udp::ErrorCode,
        >,
    > {
        let socket = self.table.get(&socket_resource)?.clone();
        let remote_address = match remote_address {
            Some(address) => match parse_p2_udp_socket_address(address, socket.family()) {
                Ok(address) => Some(address),
                Err(error) => return Ok(Err(map_p2_udp_error(error))),
            },
            None => None,
        };
        let (incoming, outgoing) = match socket.open_p2_streams(remote_address) {
            Ok(streams) => streams,
            Err(error) => return Ok(Err(map_p2_udp_error(error))),
        };
        let incoming = self.table.push_child(incoming, &socket_resource)?;
        let outgoing = self.table.push_child(outgoing, &socket_resource)?;
        Ok(Ok((incoming, outgoing)))
    }

    fn local_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<p2udp::IpSocketAddress, p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .local_address()
            .map(format_p2_udp_socket_address)
            .map_err(map_p2_udp_error))
    }

    fn remote_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<p2udp::IpSocketAddress, p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .remote_address()
            .map(format_p2_udp_socket_address)
            .map_err(map_p2_udp_error))
    }

    fn address_family(&mut self, socket: Resource<UdpSocket>) -> Result<p2udp::IpAddressFamily> {
        let socket = self.table.get(&socket)?.clone();
        Ok(format_p2_udp_family(socket.family()))
    }

    fn unicast_hop_limit(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u8, p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.unicast_hop_limit().map_err(map_p2_udp_error))
    }

    fn set_unicast_hop_limit(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u8,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_unicast_hop_limit(value)
            .map_err(map_p2_udp_error))
    }

    fn receive_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.receive_buffer_size().map_err(map_p2_udp_error))
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_receive_buffer_size(value)
            .map_err(map_p2_udp_error))
    }

    fn send_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.send_buffer_size().map_err(map_p2_udp_error))
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), p2udp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_send_buffer_size(value).map_err(map_p2_udp_error))
    }

    fn subscribe(&mut self, socket: Resource<UdpSocket>) -> Result<Resource<p2udp::Pollable>> {
        subscribe(&mut self.table, socket)
    }

    fn drop(&mut self, resource: Resource<UdpSocket>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2udp::HostIncomingDatagramStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn receive(
        &mut self,
        resource: Resource<p2udp::IncomingDatagramStream>,
        max_results: u64,
    ) -> Result<core::result::Result<Vec<p2udp::IncomingDatagram>, p2udp::ErrorCode>> {
        let (socket, remote_address) = {
            let stream = self.table.get(&resource)?;
            (stream.socket.clone(), stream.remote_address)
        };
        let max_results = usize::try_from(max_results).unwrap_or(usize::MAX);
        if max_results == 0 {
            return Ok(Ok(Vec::new()));
        }

        let mut datagrams = Vec::new();
        while datagrams.len() < max_results {
            match socket.receive_datagram(remote_address, 0).await {
                Ok(datagram) => datagrams.push(p2udp::IncomingDatagram {
                    data: datagram.bytes,
                    remote_address: format_p2_udp_socket_address(datagram.remote_address),
                }),
                Err(WasiUdpSocketError::WouldBlock) => break,
                Err(error) if !datagrams.is_empty() => {
                    if matches!(error, WasiUdpSocketError::WouldBlock) {
                        break;
                    }
                    return Ok(Ok(datagrams));
                }
                Err(error) => return Ok(Err(map_p2_udp_error(error))),
            }
        }
        Ok(Ok(datagrams))
    }

    fn subscribe(
        &mut self,
        resource: Resource<p2udp::IncomingDatagramStream>,
    ) -> Result<Resource<p2udp::Pollable>> {
        subscribe(&mut self.table, resource)
    }

    fn drop(&mut self, resource: Resource<p2udp::IncomingDatagramStream>) -> Result<()> {
        self.table.get(&resource)?.socket.release_stream_handle();
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2udp::HostOutgoingDatagramStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn check_send(
        &mut self,
        resource: Resource<p2udp::OutgoingDatagramStream>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        let stream = self.table.get_mut(&resource)?;
        stream.check_send_permit_count = 1;
        Ok(Ok(1))
    }

    async fn send(
        &mut self,
        resource: Resource<p2udp::OutgoingDatagramStream>,
        datagrams: Vec<p2udp::OutgoingDatagram>,
    ) -> Result<core::result::Result<u64, p2udp::ErrorCode>> {
        if datagrams.is_empty() {
            return Ok(Ok(0));
        }
        let (socket, connected_remote) = {
            let stream = self.table.get_mut(&resource)?;
            if datagrams.len() > stream.check_send_permit_count as usize {
                return Err(wasmtime::Error::new(WasiAdapterTrap::UdpSendPermitExceeded));
            }
            stream.check_send_permit_count -= datagrams.len() as u64;
            (stream.socket.clone(), stream.remote_address)
        };

        let mut sent = 0_u64;
        for datagram in datagrams {
            let remote_address = match (connected_remote, datagram.remote_address) {
                (Some(connected), None) => Some(connected),
                (Some(connected), Some(provided)) => {
                    let provided = match parse_p2_udp_socket_address(provided, socket.family()) {
                        Ok(address) => address,
                        Err(error) => return Ok(Err(map_p2_udp_error(error))),
                    };
                    if connected == provided {
                        Some(connected)
                    } else {
                        return Ok(Err(p2udp::ErrorCode::InvalidArgument));
                    }
                }
                (None, Some(provided)) => {
                    let provided = match parse_p2_udp_socket_address(provided, socket.family()) {
                        Ok(address) => address,
                        Err(error) => return Ok(Err(map_p2_udp_error(error))),
                    };
                    Some(provided)
                }
                _ => return Ok(Err(p2udp::ErrorCode::InvalidArgument)),
            };
            match socket
                .send_datagram(&datagram.data, remote_address, 0)
                .await
            {
                Ok(()) => sent += 1,
                Err(WasiUdpSocketError::WouldBlock) if sent == 0 => return Ok(Ok(0)),
                Err(WasiUdpSocketError::WouldBlock) => return Ok(Ok(sent)),
                Err(error) if sent == 0 => return Ok(Err(map_p2_udp_error(error))),
                Err(_) => return Ok(Ok(sent)),
            }
        }
        Ok(Ok(sent))
    }

    fn subscribe(
        &mut self,
        resource: Resource<p2udp::OutgoingDatagramStream>,
    ) -> Result<Resource<p2udp::Pollable>> {
        subscribe(&mut self.table, resource)
    }

    fn drop(&mut self, resource: Resource<p2udp::OutgoingDatagramStream>) -> Result<()> {
        self.table.get(&resource)?.socket.release_stream_handle();
        delete_resource(self, resource)
    }
}

impl<CpuImpl, HostFs> p2udp_create::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn create_udp_socket(
        &mut self,
        address_family: p2udp_create::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<UdpSocket>, p2udp_create::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::UDP) {
            return Ok(Err(p2udp_create::ErrorCode::AccessDenied));
        }
        let family = match address_family {
            p2udp_create::IpAddressFamily::Ipv4 => WasiUdpSocketFamily::Ipv4,
            p2udp_create::IpAddressFamily::Ipv6 => {
                return Ok(Err(p2udp_create::ErrorCode::NotSupported));
            }
        };
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2udp_create::ErrorCode::Unknown));
        };
        let resource = self.table.push(UdpSocket::new(service, family))?;
        Ok(Ok(resource))
    }
}

fn parse_p2_udp_socket_address(
    address: p2udp::IpSocketAddress,
    family: WasiUdpSocketFamily,
) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
    match (family, address) {
        (WasiUdpSocketFamily::Ipv4, p2udp::IpSocketAddress::Ipv4(address)) => {
            Ok(WasiUdpSocketAddress {
                address: crate::Ipv4Address::new([
                    address.address.0,
                    address.address.1,
                    address.address.2,
                    address.address.3,
                ]),
                port: address.port,
            })
        }
        (WasiUdpSocketFamily::Ipv4, p2udp::IpSocketAddress::Ipv6(_)) => {
            Err(WasiUdpSocketError::NotSupported)
        }
    }
}

fn format_p2_udp_socket_address(address: WasiUdpSocketAddress) -> p2udp::IpSocketAddress {
    let [a, b, c, d] = address.address.octets();
    p2udp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
        port: address.port,
        address: (a, b, c, d),
    })
}

fn format_p2_udp_family(family: WasiUdpSocketFamily) -> p2udp::IpAddressFamily {
    match family {
        WasiUdpSocketFamily::Ipv4 => p2udp::IpAddressFamily::Ipv4,
    }
}

fn map_p2_udp_error(error: WasiUdpSocketError) -> p2udp::ErrorCode {
    match error {
        WasiUdpSocketError::NotSupported => p2udp::ErrorCode::NotSupported,
        WasiUdpSocketError::InvalidArgument => p2udp::ErrorCode::InvalidArgument,
        WasiUdpSocketError::InvalidState => p2udp::ErrorCode::InvalidState,
        WasiUdpSocketError::NotInProgress => p2udp::ErrorCode::NotInProgress,
        WasiUdpSocketError::AddressNotBindable => p2udp::ErrorCode::AddressNotBindable,
        WasiUdpSocketError::DatagramTooLarge => p2udp::ErrorCode::DatagramTooLarge,
        WasiUdpSocketError::WouldBlock
        | WasiUdpSocketError::Backend(crate::UdpError {
            kind: crate::UdpErrorKind::Timeout,
            ..
        }) => p2udp::ErrorCode::WouldBlock,
        WasiUdpSocketError::Backend(crate::UdpError {
            kind: crate::UdpErrorKind::UnresolvedHost,
            ..
        }) => p2udp::ErrorCode::NameUnresolvable,
        WasiUdpSocketError::Backend(_) => p2udp::ErrorCode::Unknown,
    }
}

fn parse_p2_tcp_socket_address(
    address: p2tcp::IpSocketAddress,
    family: WasiTcpSocketFamily,
) -> core::result::Result<WasiTcpSocketAddress, p2tcp::ErrorCode> {
    match (family, address) {
        (WasiTcpSocketFamily::Ipv4, p2tcp::IpSocketAddress::Ipv4(address)) => {
            Ok(WasiTcpSocketAddress {
                address: crate::Ipv4Address::new([
                    address.address.0,
                    address.address.1,
                    address.address.2,
                    address.address.3,
                ]),
                port: address.port,
            })
        }
        (WasiTcpSocketFamily::Ipv4, p2tcp::IpSocketAddress::Ipv6(_)) => {
            Err(p2tcp::ErrorCode::NotSupported)
        }
    }
}

fn format_p2_tcp_socket_address(address: WasiTcpSocketAddress) -> p2tcp::IpSocketAddress {
    let [a, b, c, d] = address.address.octets();
    p2tcp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
        port: address.port,
        address: (a, b, c, d),
    })
}

fn map_p2_tcp_family(
    family: p2tcp_create::IpAddressFamily,
) -> core::result::Result<WasiTcpSocketFamily, p2tcp_create::ErrorCode> {
    match family {
        p2tcp_create::IpAddressFamily::Ipv4 => Ok(WasiTcpSocketFamily::Ipv4),
        p2tcp_create::IpAddressFamily::Ipv6 => Err(p2tcp_create::ErrorCode::NotSupported),
    }
}

fn format_p2_tcp_family(family: WasiTcpSocketFamily) -> p2tcp::IpAddressFamily {
    match family {
        WasiTcpSocketFamily::Ipv4 => p2tcp::IpAddressFamily::Ipv4,
    }
}

fn map_p2_tcp_core_error(error: crate::TcpError) -> p2tcp::ErrorCode {
    match error.kind {
        crate::TcpErrorKind::UnresolvedHost => p2tcp::ErrorCode::NameUnresolvable,
        crate::TcpErrorKind::Timeout => p2tcp::ErrorCode::Timeout,
        crate::TcpErrorKind::PermissionDenied => p2tcp::ErrorCode::AccessDenied,
        crate::TcpErrorKind::Unavailable => p2tcp::ErrorCode::RemoteUnreachable,
        crate::TcpErrorKind::Internal => p2tcp::ErrorCode::Unknown,
    }
}

fn map_p2_tcp_socket_error(error: super::socket_types::ErrorCode) -> p2tcp::ErrorCode {
    match error {
        super::socket_types::ErrorCode::AccessDenied => p2tcp::ErrorCode::AccessDenied,
        super::socket_types::ErrorCode::NotSupported => p2tcp::ErrorCode::NotSupported,
        super::socket_types::ErrorCode::InvalidArgument => p2tcp::ErrorCode::InvalidArgument,
        super::socket_types::ErrorCode::OutOfMemory => p2tcp::ErrorCode::OutOfMemory,
        super::socket_types::ErrorCode::Timeout => p2tcp::ErrorCode::Timeout,
        super::socket_types::ErrorCode::InvalidState => p2tcp::ErrorCode::InvalidState,
        super::socket_types::ErrorCode::AddressNotBindable => p2tcp::ErrorCode::AddressNotBindable,
        super::socket_types::ErrorCode::AddressInUse => p2tcp::ErrorCode::AddressInUse,
        super::socket_types::ErrorCode::RemoteUnreachable => p2tcp::ErrorCode::RemoteUnreachable,
        super::socket_types::ErrorCode::ConnectionRefused => p2tcp::ErrorCode::ConnectionRefused,
        super::socket_types::ErrorCode::ConnectionBroken => p2tcp::ErrorCode::ConnectionReset,
        super::socket_types::ErrorCode::ConnectionReset => p2tcp::ErrorCode::ConnectionReset,
        super::socket_types::ErrorCode::ConnectionAborted => p2tcp::ErrorCode::ConnectionAborted,
        super::socket_types::ErrorCode::DatagramTooLarge => p2tcp::ErrorCode::DatagramTooLarge,
        super::socket_types::ErrorCode::Other(_) => p2tcp::ErrorCode::Unknown,
    }
}

impl<CpuImpl, HostFs> p2tcp::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> p2tcp::HostTcpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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
        socket: Resource<TcpSocket>,
        network: Resource<p2tcp::Network>,
        remote_address: p2tcp::IpSocketAddress,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::TCP) {
            return Ok(Err(p2tcp::ErrorCode::AccessDenied));
        }
        let _ = self.table.get(&network)?;
        let socket = self.table.get(&socket)?.clone();
        let remote_address = match parse_p2_tcp_socket_address(remote_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(error)),
        };
        let (service, inner, ready) = {
            let mut state = socket.inner.lock();
            if state.stream.is_some() || state.connect_in_progress || state.connect_result.is_some()
            {
                return Ok(Err(p2tcp::ErrorCode::InvalidState));
            }
            state.connect_in_progress = true;
            (
                state.service.clone(),
                socket.inner.clone(),
                socket.ready.clone(),
            )
        };
        self.spawner().spawn_detached(async move {
            let mut host_buffer = [0; 15];
            let result = service
                .tcp_connect(
                    remote_address
                        .address
                        .write_dotted_decimal(&mut host_buffer),
                    remote_address.port,
                    u64::MAX,
                )
                .await;
            let mut state = inner.lock();
            state.connect_in_progress = false;
            state.connect_result = Some(match result {
                Ok(stream) => {
                    state.stream = Some(stream);
                    state.remote_address = Some(remote_address);
                    Ok(())
                }
                Err(error) => Err(error),
            });
            ready.notify_all();
        });
        Ok(Ok(()))
    }

    fn finish_connect(
        &mut self,
        socket_resource: Resource<TcpSocket>,
    ) -> Result<
        core::result::Result<
            (Resource<p2tcp::InputStream>, Resource<p2tcp::OutputStream>),
            p2tcp::ErrorCode,
        >,
    > {
        let socket = self.table.get(&socket_resource)?.clone();
        {
            let mut state = socket.inner.lock();
            if state.connect_in_progress {
                return Ok(Err(p2tcp::ErrorCode::WouldBlock));
            }
            let Some(result) = state.connect_result.take() else {
                return Ok(Err(p2tcp::ErrorCode::NotInProgress));
            };
            if let Err(error) = result {
                return Ok(Err(map_p2_tcp_core_error(error)));
            }
        }
        let (network_writer, guest_reader) = crate::byte_channel();
        let (guest_writer, network_reader) = crate::byte_channel();
        let input = self.table.push_child(
            Box::new(ChannelInputStream::new(guest_reader)) as DynInputStream,
            &socket_resource,
        )?;
        let output = self.table.push_child(
            Box::new(ChannelOutputStream::new(guest_writer)) as DynOutputStream,
            &socket_resource,
        )?;
        let read_socket = socket.clone();
        self.spawner().spawn_detached(async move {
            loop {
                match read_socket.read(super::FILE_READ_CHUNK_BYTES as u32).await {
                    Ok(Some(bytes)) => {
                        if network_writer.write(bytes).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(
                            target: "helios_kernel::wasi::preview2::tcp",
                            ?error,
                            "tcp input stream bridge stopped after backend read error"
                        );
                        break;
                    }
                }
            }
        });
        self.spawner().spawn_detached(async move {
            while let Some(bytes) = network_reader.read().await {
                if let Err(error) = socket.write_all(&bytes).await {
                    tracing::warn!(
                        target: "helios_kernel::wasi::preview2::tcp",
                        ?error,
                        "tcp output stream bridge stopped after backend write error"
                    );
                    break;
                }
            }
        });
        Ok(Ok((input, output)))
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
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::IpSocketAddress, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .remote_address()
            .map(format_p2_tcp_socket_address)
            .map_err(map_p2_tcp_socket_error))
    }

    fn is_listening(&mut self, socket: Resource<TcpSocket>) -> Result<bool> {
        let _ = self.table.get(&socket)?;
        Ok(false)
    }

    fn address_family(&mut self, socket: Resource<TcpSocket>) -> Result<p2tcp::IpAddressFamily> {
        let socket = self.table.get(&socket)?.clone();
        Ok(format_p2_tcp_family(socket.family()))
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
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .receive_buffer_size()
            .map_err(map_p2_tcp_socket_error))
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_receive_buffer_size(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn send_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.send_buffer_size().map_err(map_p2_tcp_socket_error))
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_send_buffer_size(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn subscribe(&mut self, socket: Resource<TcpSocket>) -> Result<Resource<p2tcp::Pollable>> {
        subscribe(&mut self.table, socket)
    }

    fn shutdown(
        &mut self,
        _: Resource<TcpSocket>,
        _: p2tcp::ShutdownType,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        socket_not_supported()
    }

    fn drop(&mut self, resource: Resource<TcpSocket>) -> Result<()> {
        let socket = self.table.delete(resource)?;
        if let Some((service, stream)) = socket.take_connected_stream() {
            self.spawner().spawn_detached(async move {
                service.tcp_close(stream).await;
            });
        }
        Ok(())
    }
}

impl<CpuImpl, HostFs> p2tcp_create::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn create_tcp_socket(
        &mut self,
        address_family: p2tcp_create::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<TcpSocket>, p2tcp_create::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::TCP) {
            return Ok(Err(p2tcp_create::ErrorCode::AccessDenied));
        }
        let family = match map_p2_tcp_family(address_family) {
            Ok(family) => family,
            Err(error) => return Ok(Err(error)),
        };
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2tcp_create::ErrorCode::Unknown));
        };
        let resource = self.table.push(TcpSocket::new(service, family))?;
        Ok(Ok(resource))
    }
}

impl<CpuImpl, HostFs> p2lookup::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn resolve_addresses(
        &mut self,
        _: Resource<p2lookup::Network>,
        name: String,
    ) -> Result<core::result::Result<Resource<p2lookup::ResolveAddressStream>, p2lookup::ErrorCode>>
    {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::DNS) {
            return Ok(Err(p2lookup::ErrorCode::AccessDenied));
        }
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2lookup::ErrorCode::TemporaryResolverFailure));
        };
        let stream = P2ResolveAddressStream::pending();
        let task_stream = stream.clone();
        self.spawner().spawn_detached(async move {
            task_stream.complete(service.dns_resolve(&name, u64::MAX).await);
        });
        let resource = self.table.push(stream)?;
        Ok(Ok(resource))
    }
}

impl<CpuImpl, HostFs> p2lookup::HostResolveAddressStream for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn resolve_next_address(
        &mut self,
        resource: Resource<p2lookup::ResolveAddressStream>,
    ) -> Result<core::result::Result<Option<p2lookup::IpAddress>, p2lookup::ErrorCode>> {
        let stream = self.table.get_mut(&resource)?;
        if !stream.is_ready() {
            return Ok(Err(p2lookup::ErrorCode::WouldBlock));
        }
        Ok(stream
            .next_address()
            .map(|address| address.map(format_p2_ip_address))
            .map_err(map_p2_dns_error))
    }

    fn subscribe(
        &mut self,
        resource: Resource<p2lookup::ResolveAddressStream>,
    ) -> Result<Resource<p2lookup::Pollable>> {
        subscribe(&mut self.table, resource)
    }

    fn drop(&mut self, resource: Resource<p2lookup::ResolveAddressStream>) -> Result<()> {
        delete_resource(self, resource)
    }
}

fn format_p2_ip_address(address: crate::Ipv4Address) -> p2lookup::IpAddress {
    let [a, b, c, d] = address.octets();
    p2lookup::IpAddress::Ipv4((a, b, c, d))
}

fn map_p2_dns_error(error: crate::DnsError) -> p2lookup::ErrorCode {
    match error.kind {
        crate::DnsErrorKind::UnresolvedHost => p2lookup::ErrorCode::NameUnresolvable,
        crate::DnsErrorKind::Timeout | crate::DnsErrorKind::Unavailable => {
            p2lookup::ErrorCode::TemporaryResolverFailure
        }
        crate::DnsErrorKind::Internal => p2lookup::ErrorCode::PermanentResolverFailure,
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
    CpuImpl: Cpu + Clone,
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
        FsNodeKind::Symlink => p2fs::DescriptorType::SymbolicLink,
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

fn datetime_from_p3(instant: P3SystemInstant) -> p2fs::Datetime {
    p2fs::Datetime {
        seconds: instant
            .seconds
            .try_into()
            .expect("preview2 filesystem cannot represent a negative timestamp"),
        nanoseconds: instant.nanoseconds,
    }
}

fn p2_new_timestamp_nanos(
    timestamp: p2fs::NewTimestamp,
    now_nanos: u64,
) -> core::result::Result<Option<u64>, p2fs::ErrorCode> {
    match timestamp {
        p2fs::NewTimestamp::NoChange => Ok(None),
        p2fs::NewTimestamp::Now => Ok(Some(now_nanos)),
        p2fs::NewTimestamp::Timestamp(datetime) => {
            let nanos = datetime
                .seconds
                .checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add(u64::from(datetime.nanoseconds)))
                .ok_or(p2fs::ErrorCode::Overflow)?;
            Ok(Some(nanos))
        }
    }
}

fn error_code_from_path(error: crate::ComponentFsPathError) -> p2fs::ErrorCode {
    match error {
        crate::ComponentFsPathError::InvalidBasePath => p2fs::ErrorCode::Invalid,
        crate::ComponentFsPathError::NotPermitted => p2fs::ErrorCode::NotPermitted,
    }
}

fn p2_stream_error(error: p2fs::ErrorCode) -> StreamError {
    StreamError::LastOperationFailed(wasmtime::Error::new(P2FileStreamError::WriteFailed(error)))
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

#[cfg(test)]
mod tests {
    use super::{
        WasiTcpSocketFamily, map_p2_tcp_core_error, map_p2_tcp_socket_error, p2net, p2tcp,
        parse_p2_tcp_socket_address,
    };

    #[test]
    fn p2_tcp_maps_core_network_errors_without_preview3_intermediate() {
        assert_eq!(
            map_p2_tcp_core_error(crate::TcpError {
                kind: crate::TcpErrorKind::UnresolvedHost,
                detail: crate::NetworkErrorDetail::DnsLookupFailed,
            }),
            p2tcp::ErrorCode::NameUnresolvable
        );
        assert_eq!(
            map_p2_tcp_core_error(crate::TcpError {
                kind: crate::TcpErrorKind::Timeout,
                detail: crate::NetworkErrorDetail::TcpConnectTimeout,
            }),
            p2tcp::ErrorCode::Timeout
        );
        assert_eq!(
            map_p2_tcp_socket_error(super::super::socket_types::ErrorCode::InvalidState),
            p2tcp::ErrorCode::InvalidState
        );
    }

    #[test]
    fn p2_tcp_ipv4_socket_rejects_ipv6_addresses() {
        let ipv4 = parse_p2_tcp_socket_address(
            p2tcp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
                port: 80,
                address: (127, 0, 0, 1),
            }),
            WasiTcpSocketFamily::Ipv4,
        )
        .expect("IPv4 address should parse for IPv4 socket");
        assert_eq!(ipv4.port, 80);

        let ipv6 = parse_p2_tcp_socket_address(
            p2tcp::IpSocketAddress::Ipv6(p2net::Ipv6SocketAddress {
                port: 80,
                flow_info: 0,
                address: (0, 0, 0, 0, 0, 0, 0, 1),
                scope_id: 0,
            }),
            WasiTcpSocketFamily::Ipv4,
        )
        .expect_err("IPv6 address must not be accepted by IPv4 socket");
        assert_eq!(ipv6, p2tcp::ErrorCode::NotSupported);
    }
}
