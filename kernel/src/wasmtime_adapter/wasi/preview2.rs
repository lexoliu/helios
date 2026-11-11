use crate::ComponentHostNetwork;
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use helios_netstack::Ipv6Address;
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
use super::net::{TcpSocketInputStream, TcpSocketOutputStream};
use super::{
    FileWriteMode, FsDescriptor, FsNodeKind, P2IncomingDatagramStream, P2Network,
    P2OutgoingDatagramStream, P2ResolveAddressStream, PendingAccept, Preview2GuestExit, TcpSocket,
    UdpSocket, WasiAdapterTrap, WasiImportSet, WasiTcpSocketAddress, WasiTcpSocketFamily,
    WasiUdpSocketAddress, WasiUdpSocketError, WasiUdpSocketFamily, has_wasi_network_rights,
    output_is_terminal, stdin_is_terminal, wasi_tcp_bind_rights, wasi_udp_bind_rights,
};
use crate::wasmtime_adapter::component_host::{
    HostRuntimeState, RuntimeDeadlinePollable, StoreData,
};
use crate::wasmtime_adapter::store::{
    ChannelInputStream, ChannelOutputStream, SerialOutputStream, StdioOutputStream,
};
use crate::{
    ComponentOutputMode, ComponentOutputRoute, ComponentOutputStreamKind, HeldPath, Namespace,
    NetworkHandle, ProfileScope,
};

#[cfg(test)]
pub(crate) const PREVIEW2_LINKED_INTERFACES: &[&str] = &[
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
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
        "wasi:io",
        include_str!("../../../../../wasmtime/crates/wasi/src/p2/wit/deps/io.wit"),
    ),
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
                        import wasi:cli/environment@0.2.12;
                        import wasi:cli/exit@0.2.12;
                        import wasi:cli/stdin@0.2.12;
                        import wasi:cli/stdout@0.2.12;
                        import wasi:cli/stderr@0.2.12;
                        import wasi:cli/terminal-input@0.2.12;
                        import wasi:cli/terminal-output@0.2.12;
                        import wasi:cli/terminal-stdin@0.2.12;
                        import wasi:cli/terminal-stdout@0.2.12;
                        import wasi:cli/terminal-stderr@0.2.12;
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
                        import wasi:clocks/monotonic-clock@0.2.12;
                        import wasi:clocks/wall-clock@0.2.12;
                        import wasi:clocks/timezone@0.2.12;
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
                        import wasi:filesystem/preopens@0.2.12;
                        import wasi:filesystem/types@0.2.12;
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
                "wasi:filesystem/types.[method]descriptor.set-times": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.set-times-at": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.sync": async | tracing | trappable,
                "wasi:filesystem/types.[method]descriptor.sync-data": async | tracing | trappable,
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
                        import wasi:random/random@0.2.12;
                        import wasi:random/insecure@0.2.12;
                        import wasi:random/insecure-seed@0.2.12;
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
                        import wasi:sockets/network@0.2.12;
                        import wasi:sockets/instance-network@0.2.12;
                        import wasi:sockets/udp@0.2.12;
                        import wasi:sockets/udp-create-socket@0.2.12;
                        import wasi:sockets/tcp@0.2.12;
                        import wasi:sockets/tcp-create-socket@0.2.12;
                        import wasi:sockets/ip-name-lookup@0.2.12;
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

/// Bytes a single `write` permit covers on a preview2 file output stream.
/// Bytes one `ready` moves for a preview2 file stream: what a write permit
/// covers on an output stream, and what one read pulls into an input
/// stream. One chunk is one filesystem operation, so a host-share
/// transfer stays well below the 9p message size and a stream never holds
/// more than one chunk of a file.
const P2_FILE_STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub struct TerminalInput;
pub struct TerminalOutput;
pub struct DirectoryEntryStream {
    entries: Vec<p2fs::DirectoryEntry>,
    cursor: usize,
}

struct EmptyInputStream;

/// Reads a file through the mount that serves it, one bounded chunk at a
/// time.
///
/// `wasi:io/streams.input-stream.read` is synchronous, so the read happens
/// in `Pollable::ready`, which the guest awaits through the stream's
/// pollable (and which `blocking-read` awaits for it). Only one chunk is
/// ever resident, so streaming a large file never scales kernel memory
/// with the file's size.
struct FileInputStream<HostFs> {
    file: HeldPath<HostFs>,
    offset: u64,
    buffer: Bytes,
    end_of_file: bool,
    failure: Option<p2fs::ErrorCode>,
}

/// Writes a file through the mount that serves it.
///
/// `write` must never block, and reaching a filesystem may take a device
/// or host round trip, so each permitted batch is parked here and handed
/// to the filesystem from `Pollable::ready`; `check_write` withholds the
/// next permit until then.
struct FileOutputStream<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    cpu: CpuImpl,
    runtime_state: HostRuntimeState<CpuImpl, Net, HostFs>,
    file: HeldPath<HostFs>,
    mode: FileWriteMode,
    pending: Option<Bytes>,
    failure: Option<p2fs::ErrorCode>,
}

#[derive(Debug, Error)]
enum P2FileStreamError {
    #[error("preview2 file stream operation failed: {0:?}")]
    Failed(p2fs::ErrorCode),
}

impl<HostFs> FileInputStream<HostFs>
where
    HostFs: crate::HostFileSystem,
{
    fn new(file: HeldPath<HostFs>, offset: u64) -> Self {
        Self {
            file,
            offset,
            buffer: Bytes::new(),
            end_of_file: false,
            failure: None,
        }
    }

    async fn fill(&mut self) {
        if !self.buffer.is_empty() || self.end_of_file || self.failure.is_some() {
            return;
        }
        match super::read_resolved(
            &self.file.resolved(),
            self.offset,
            P2_FILE_STREAM_CHUNK_BYTES,
        )
        .await
        {
            Ok(bytes) if bytes.is_empty() => self.end_of_file = true,
            Ok(bytes) => {
                self.offset = self
                    .offset
                    .checked_add(u64::try_from(bytes.len()).expect("chunk length overflowed u64"))
                    .expect("file stream offset overflowed u64");
                self.buffer = bytes;
            }
            Err(error) => self.failure = Some(error_code_from_p3(error)),
        }
    }
}

impl<CpuImpl, Net, HostFs> FileOutputStream<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn new(
        cpu: CpuImpl,
        runtime_state: HostRuntimeState<CpuImpl, Net, HostFs>,
        file: HeldPath<HostFs>,
        mode: FileWriteMode,
    ) -> Self {
        Self {
            cpu,
            runtime_state,
            file,
            mode,
            pending: None,
            failure: None,
        }
    }

    fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    /// Hands the parked batch to the filesystem.
    ///
    /// Positional writes advance the stream offset by what was written;
    /// append writes let the filesystem find the end of the file when the
    /// batch arrives, so a file another writer grew meanwhile is not
    /// clobbered.
    async fn flush_pending(&mut self) {
        let Some(bytes) = self.pending.take() else {
            return;
        };
        let now = self.now_nanos();
        match super::write_resolved(&self.file.resolved(), self.mode, &bytes, now).await {
            Ok(end) => {
                if let FileWriteMode::At(offset) = &mut self.mode {
                    *offset = end;
                }
            }
            Err(error) => self.failure = Some(error_code_from_p3(error)),
        }
    }
}

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    /// A descriptor of this store's table and the store's namespace, in
    /// preview2's error codes.
    fn p2_descriptor(
        &mut self,
        resource: &Resource<FsDescriptor>,
    ) -> core::result::Result<(FsDescriptor, Namespace<HostFs>), p2fs::ErrorCode> {
        super::descriptor_in(self, resource).map_err(error_code_from_p3)
    }

    /// A file stream's target: `resource` must name a regular file opened
    /// with `right`.
    fn p2_stream_target(
        &mut self,
        resource: &Resource<FsDescriptor>,
        right: p3fs::DescriptorFlags,
        refused: p3fs::ErrorCode,
    ) -> core::result::Result<HeldPath<HostFs>, p2fs::ErrorCode> {
        let (descriptor, namespace) = self.p2_descriptor(resource)?;
        super::stream_target(&namespace, &descriptor, right, refused).map_err(error_code_from_p3)
    }

    fn p2_output_stream(
        &mut self,
        resource: &Resource<FsDescriptor>,
        mode: FileWriteMode,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        let file = match self.p2_stream_target(
            resource,
            p3fs::DescriptorFlags::WRITE,
            p3fs::ErrorCode::NotPermitted,
        ) {
            Ok(file) => file,
            Err(error) => return Ok(Err(error)),
        };
        let stream =
            FileOutputStream::new(self.cpu.clone(), self.runtime_state.clone(), file, mode);
        Ok(Ok(self.table.push(Box::new(stream) as DynOutputStream)?))
    }

    /// Backs `set-times` and `set-times-at`.
    async fn set_times_at_absolute_path(
        &mut self,
        namespace: Namespace<HostFs>,
        absolute: String,
        data_access_timestamp: p2fs::NewTimestamp,
        data_modification_timestamp: p2fs::NewTimestamp,
    ) -> core::result::Result<(), p2fs::ErrorCode> {
        let now_nanos = self.system_time_nanos();
        let access_nanos = p2_new_timestamp_nanos(data_access_timestamp, now_nanos)?;
        let modified_nanos = p2_new_timestamp_nanos(data_modification_timestamp, now_nanos)?;
        super::set_times(
            &namespace,
            &absolute,
            access_nanos,
            modified_nanos,
            now_nanos,
        )
        .await
        .map_err(error_code_from_p3)
    }
}

pub(crate) fn add_to_linker<CpuImpl, Net, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, Net, HostFs>>,
    imports: &WasiImportSet,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if imports.has("wasi:cli/environment", "0.2") {
        cli_bindings::cli::environment::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/exit", "0.2") {
        cli_bindings::cli::exit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdin", "0.2") {
        cli_bindings::cli::stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdout", "0.2") {
        cli_bindings::cli::stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stderr", "0.2") {
        cli_bindings::cli::stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-input", "0.2") {
        cli_bindings::cli::terminal_input::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:cli/terminal-output", "0.2") {
        cli_bindings::cli::terminal_output::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:cli/terminal-stdin", "0.2") {
        cli_bindings::cli::terminal_stdin::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:cli/terminal-stdout", "0.2") {
        cli_bindings::cli::terminal_stdout::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:cli/terminal-stderr", "0.2") {
        cli_bindings::cli::terminal_stderr::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }

    if imports.has("wasi:clocks/monotonic-clock", "0.2") {
        clocks_bindings::clocks::monotonic_clock::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:clocks/wall-clock", "0.2") {
        clocks_bindings::clocks::wall_clock::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:clocks/timezone", "0.2") {
        // Every member is gated behind the `clocks-timezone` unstable
        // feature; opt in or the interface links with no functions.
        let mut options = clocks_bindings::clocks::timezone::LinkOptions::default();
        options.clocks_timezone(true);
        clocks_bindings::clocks::timezone::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, &options, |state| state)?;
    }

    if imports.has("wasi:filesystem/preopens", "0.2") {
        filesystem_bindings::filesystem::preopens::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:filesystem/types", "0.2") {
        filesystem_bindings::filesystem::types::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }

    if imports.has("wasi:random/random", "0.2") {
        random_bindings::random::random::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:random/insecure", "0.2") {
        random_bindings::random::insecure::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:random/insecure-seed", "0.2") {
        random_bindings::random::insecure_seed::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/network", "0.2") {
        sockets_bindings::sockets::network::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, &Default::default(), |state| state)?;
    }
    if imports.has("wasi:sockets/instance-network", "0.2") {
        sockets_bindings::sockets::instance_network::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/udp", "0.2") {
        sockets_bindings::sockets::udp::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/udp-create-socket", "0.2") {
        sockets_bindings::sockets::udp_create_socket::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/tcp", "0.2") {
        sockets_bindings::sockets::tcp::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/tcp-create-socket", "0.2") {
        sockets_bindings::sockets::tcp_create_socket::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    if imports.has("wasi:sockets/ip-name-lookup", "0.2") {
        sockets_bindings::sockets::ip_name_lookup::add_to_linker::<
            _,
            HasSelf<StoreData<CpuImpl, Net, HostFs>>,
        >(linker, |state| state)?;
    }
    Ok(())
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for EmptyInputStream {
    /// A closed stream is always ready: there is nothing left to wait for.
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for EmptyInputStream {
    /// Signal end-of-file the same way `FileInputStream` does once its
    /// contents are drained. Returning an empty `Bytes` instead would mean
    /// "no data ready yet", which makes `blocking_read` spin until it
    /// exhausts `MAX_BLOCKING_ATTEMPTS` and traps the guest.
    fn read(&mut self, _: usize) -> core::result::Result<Bytes, StreamError> {
        Err(StreamError::Closed)
    }
}

#[wasmtime_wasi_io::async_trait]
impl<HostFs> Pollable for FileInputStream<HostFs>
where
    HostFs: crate::HostFileSystem,
{
    async fn ready(&mut self) {
        self.fill().await;
    }
}

#[wasmtime_wasi_io::async_trait]
impl<HostFs> InputStream for FileInputStream<HostFs>
where
    HostFs: crate::HostFileSystem,
{
    fn read(&mut self, size: usize) -> core::result::Result<Bytes, StreamError> {
        // A failure is sticky: once a read has failed the stream is broken,
        // and reporting it only once would let the guest read on as though
        // the file had simply ended.
        if let Some(error) = self.failure {
            return Err(p2_stream_error(error));
        }
        if self.buffer.is_empty() {
            // End of file closes the stream; otherwise the next chunk is
            // still to be read and an empty read means "nothing ready yet".
            return if self.end_of_file {
                Err(StreamError::Closed)
            } else {
                Ok(Bytes::new())
            };
        }
        let end = size.min(self.buffer.len());
        Ok(self.buffer.split_to(end))
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, Net, HostFs> Pollable for FileOutputStream<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn ready(&mut self) {
        self.flush_pending().await;
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, Net, HostFs> OutputStream for FileOutputStream<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn write(&mut self, bytes: Bytes) -> core::result::Result<(), StreamError> {
        // Sticky, like the read side: a stream whose write failed stays
        // failed instead of silently accepting more bytes.
        if let Some(error) = self.failure {
            return Err(p2_stream_error(error));
        }
        if self.pending.is_some() {
            return Err(StreamError::trap(
                "preview2 file stream write exceeded its check-write permit",
            ));
        }
        self.pending = Some(bytes);
        Ok(())
    }

    fn flush(&mut self) -> core::result::Result<(), StreamError> {
        // The parked batch is drained by `ready`, which `check_write` pends
        // on, so there is nothing left to push here.
        if let Some(error) = self.failure {
            return Err(p2_stream_error(error));
        }
        Ok(())
    }

    fn check_write(&mut self) -> core::result::Result<usize, StreamError> {
        if let Some(error) = self.failure {
            return Err(p2_stream_error(error));
        }
        if self.pending.is_some() {
            return Ok(0);
        }
        Ok(P2_FILE_STREAM_CHUNK_BYTES)
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for TcpSocket {
    /// Resolves once the socket's outstanding operation has a result.
    ///
    /// `wasi:sockets/tcp` funnels `finish-connect`, `finish-listen`, and
    /// `accept` through this one pollable: each returns `would-block` while
    /// its background task runs, and the guest is expected to block on the
    /// pollable until retrying is worthwhile. Reporting ready while an accept
    /// is still waiting for a connection turns that contract into a spin —
    /// the guest retries `accept`, gets `would-block`, and polls again — so
    /// readiness has to cover every in-flight operation, not just connect.
    async fn ready(&mut self) {
        loop {
            // Armed before the socket state is read: a background
            // connect/listen/accept broadcasts its result to every
            // pollable and banks nothing, so a wait created after the
            // task finished would park until the next operation.
            let settled = self.ready.notified();
            {
                let state = self.inner.lock();
                let in_flight = state.connect_in_progress
                    || state.listen_in_progress
                    || (state.accept_in_progress && state.accept_result.is_none());
                if !in_flight {
                    return;
                }
            }
            settled.await;
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

impl<CpuImpl, Net, HostFs> cli_bindings::cli::environment::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

impl<CpuImpl, Net, HostFs> cli_bindings::cli::exit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

impl<CpuImpl, Net, HostFs> cli_bindings::cli::stdin::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_stdin(&mut self) -> Result<Resource<DynInputStream>> {
        let stream: DynInputStream = match self.output_mode() {
            ComponentOutputMode::Child { stdin_rx, .. }
            | ComponentOutputMode::RoutedChild { stdin_rx, .. } => {
                Box::new(ChannelInputStream::new(stdin_rx.clone())) as DynInputStream
            }
            ComponentOutputMode::Serial | ComponentOutputMode::Trace => {
                Box::new(EmptyInputStream) as DynInputStream
            }
        };
        Ok(self.table.push(stream)?)
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::stdout::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_stdout(&mut self) -> Result<Resource<DynOutputStream>> {
        let stream = build_stdio_stream(self, ComponentOutputStreamKind::Stdout);
        Ok(self.table.push(Box::new(stream) as DynOutputStream)?)
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::stderr::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_stderr(&mut self) -> Result<Resource<DynOutputStream>> {
        let stream = build_stdio_stream(self, ComponentOutputStreamKind::Stderr);
        Ok(self.table.push(Box::new(stream) as DynOutputStream)?)
    }
}

fn build_stdio_stream<CpuImpl, Net, HostFs>(
    store: &StoreData<CpuImpl, Net, HostFs>,
    kind: ComponentOutputStreamKind,
) -> StdioOutputStream
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
        ComponentOutputMode::RoutedChild { stdout, stderr, .. } => {
            let route = match kind {
                ComponentOutputStreamKind::Stdout => stdout,
                ComponentOutputStreamKind::Stderr => stderr,
            };
            match route {
                ComponentOutputRoute::Child(writer) => {
                    StdioOutputStream::Child(ChannelOutputStream::new(writer.clone()))
                }
                ComponentOutputRoute::Serial => {
                    StdioOutputStream::Serial(SerialOutputStream::new(store.serial_writer()))
                }
                ComponentOutputRoute::Trace | ComponentOutputRoute::Discard => {
                    StdioOutputStream::Trace
                }
            }
        }
        ComponentOutputMode::Serial => {
            StdioOutputStream::Serial(SerialOutputStream::new(store.serial_writer()))
        }
        ComponentOutputMode::Trace => StdioOutputStream::Trace,
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_input::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_output::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_input::HostTerminalInput
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_output::HostTerminalOutput
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_stdin::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        if !stdin_is_terminal(self.output_mode()) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalInput)?))
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_stdout::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        if !output_is_terminal(self.output_mode(), ComponentOutputStreamKind::Stdout) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalOutput)?))
    }
}

impl<CpuImpl, Net, HostFs> cli_bindings::cli::terminal_stderr::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        if !output_is_terminal(self.output_mode(), ComponentOutputStreamKind::Stderr) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalOutput)?))
    }
}

impl<CpuImpl, Net, HostFs> clocks_bindings::clocks::monotonic_clock::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

impl<CpuImpl, Net, HostFs> clocks_bindings::clocks::wall_clock::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

impl<CpuImpl, Net, HostFs> clocks_bindings::clocks::timezone::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn display(
        &mut self,
        _when: clocks_bindings::clocks::timezone::Datetime,
    ) -> Result<clocks_bindings::clocks::timezone::TimezoneDisplay> {
        // The kernel does not expose a local time zone; per the WASI
        // contract that means UTC with a zero offset.
        Ok(clocks_bindings::clocks::timezone::TimezoneDisplay {
            utc_offset: 0,
            name: String::from("UTC"),
            in_daylight_saving_time: false,
        })
    }

    fn utc_offset(&mut self, _when: clocks_bindings::clocks::timezone::Datetime) -> Result<i32> {
        Ok(0)
    }
}

impl<CpuImpl, Net, HostFs> filesystem_bindings::filesystem::preopens::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

impl<CpuImpl, Net, HostFs> filesystem_bindings::filesystem::types::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn filesystem_error_code(&mut self, _: Resource<IoError>) -> Result<Option<p2fs::ErrorCode>> {
        Ok(None)
    }
}

impl<CpuImpl, Net, HostFs> filesystem_bindings::filesystem::types::HostDescriptor
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<core::result::Result<Resource<DynInputStream>, p2fs::ErrorCode>> {
        let file = match self.p2_stream_target(
            &descriptor,
            p3fs::DescriptorFlags::READ,
            p3fs::ErrorCode::NotPermitted,
        ) {
            Ok(file) => file,
            Err(error) => return Ok(Err(error)),
        };
        let stream = FileInputStream::new(file, offset);
        Ok(Ok(self.table.push(Box::new(stream) as DynInputStream)?))
    }

    fn write_via_stream(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        self.p2_output_stream(&descriptor, FileWriteMode::At(offset))
    }

    fn append_via_stream(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        self.p2_output_stream(&descriptor, FileWriteMode::Append)
    }

    fn advise(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: p2fs::Advice,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(self.p2_descriptor(&descriptor).map(drop))
    }

    async fn sync_data(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (descriptor, namespace) = target?;
            super::sync(&namespace, &descriptor, true)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    fn get_flags(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorFlags, p2fs::ErrorCode>> {
        Ok(self
            .p2_descriptor(&descriptor)
            .map(|(descriptor, _)| descriptor_flags_from_p3(descriptor.flags)))
    }

    fn get_type(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorType, p2fs::ErrorCode>> {
        Ok(self
            .p2_descriptor(&descriptor)
            .map(|(descriptor, _)| descriptor_type_from_kind(descriptor.kind)))
    }

    async fn set_size(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        size: u64,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        let now = self.now_nanos();
        Ok(async {
            let (descriptor, namespace) = target?;
            super::set_size(&namespace, &descriptor, size, now)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn set_times(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        data_access_timestamp: p2fs::NewTimestamp,
        data_modification_timestamp: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let (descriptor, namespace) = match self.p2_descriptor(&descriptor) {
            Ok(target) => target,
            Err(error) => return Ok(Err(error)),
        };
        Ok(self
            .set_times_at_absolute_path(
                namespace,
                descriptor.path,
                data_access_timestamp,
                data_modification_timestamp,
            )
            .await)
    }

    async fn read(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        length: u64,
        offset: u64,
    ) -> Result<core::result::Result<(Vec<u8>, bool), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (descriptor, namespace) = target?;
            let length = usize::try_from(length).map_err(|_| p2fs::ErrorCode::Overflow)?;
            let bytes = super::read(&namespace, &descriptor, offset, length)
                .await
                .map_err(error_code_from_p3)?;
            let end_of_file = bytes.len() < length;
            Ok((bytes.to_vec(), end_of_file))
        }
        .await)
    }

    async fn write(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        buffer: Vec<u8>,
        offset: u64,
    ) -> Result<core::result::Result<u64, p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        let now = self.now_nanos();
        Ok(async {
            let (descriptor, namespace) = target?;
            super::write(
                &namespace,
                &descriptor,
                FileWriteMode::At(offset),
                &buffer,
                now,
            )
            .await
            .map_err(error_code_from_p3)?;
            Ok(u64::try_from(buffer.len()).expect("write length overflowed u64"))
        }
        .await)
    }

    async fn read_directory(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DirectoryEntryStream>, p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        let entries = async {
            let (descriptor, namespace) = target?;
            super::read_directory(&namespace, &descriptor)
                .await
                .map_err(error_code_from_p3)
        }
        .await;
        match entries {
            Ok(entries) => Ok(Ok(self.table.push(DirectoryEntryStream {
                entries: entries.into_iter().map(directory_entry_from_p3).collect(),
                cursor: 0,
            })?)),
            Err(error) => Ok(Err(error)),
        }
    }

    async fn sync(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (descriptor, namespace) = target?;
            super::sync(&namespace, &descriptor, false)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn create_directory_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        let now = self.now_nanos();
        Ok(async {
            let (base, namespace) = target?;
            super::create_directory_at(&namespace, &base, &path, now)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn stat(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorStat, p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (descriptor, namespace) = target?;
            let metadata = super::stat_path(&namespace, &descriptor.path, crate::Follow::No)
                .await
                .map_err(error_code_from_p3)?;
            Ok(descriptor_stat_from_p3(super::descriptor_stat(&metadata)))
        }
        .await)
    }

    async fn stat_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
    ) -> Result<core::result::Result<p2fs::DescriptorStat, p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (base, namespace) = target?;
            let metadata = super::stat_at(&namespace, &base, path_flags_to_p3(path_flags), &path)
                .await
                .map_err(error_code_from_p3)?;
            Ok(descriptor_stat_from_p3(super::descriptor_stat(&metadata)))
        }
        .await)
    }

    async fn set_times_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        path: String,
        data_access_timestamp: p2fs::NewTimestamp,
        data_modification_timestamp: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self
            .p2_descriptor(&descriptor)
            .and_then(|(base, namespace)| {
                super::child_path(&base, &path)
                    .map(|absolute| (absolute, namespace))
                    .map_err(error_code_from_p3)
            });
        let (absolute, namespace) = match target {
            Ok(target) => target,
            Err(error) => return Ok(Err(error)),
        };
        Ok(self
            .set_times_at_absolute_path(
                namespace,
                absolute,
                data_access_timestamp,
                data_modification_timestamp,
            )
            .await)
    }

    async fn link_at(
        &mut self,
        source_descriptor: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let authority = self.process_authority();
        if authority.derive_link_source_cap().is_err()
            || authority.derive_link_target_directory_cap().is_err()
        {
            return Ok(Err(p2fs::ErrorCode::NotPermitted));
        }
        let target = super::descriptor_pair_in(self, &source_descriptor, &destination_descriptor)
            .map_err(error_code_from_p3);
        let now = self.now_nanos();
        Ok(async {
            let (source_base, destination_base, namespace) = target?;
            super::link_at(
                &namespace,
                &source_base,
                &source_path,
                &destination_base,
                &destination_path,
                now,
            )
            .await
            .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn open_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
        open_flags: p2fs::OpenFlags,
        flags: p2fs::DescriptorFlags,
    ) -> Result<core::result::Result<Resource<FsDescriptor>, p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        let now = self.now_nanos();
        let opened = async {
            let (base, namespace) = target?;
            super::open_at(
                &namespace,
                &base,
                path_flags_to_p3(path_flags),
                &path,
                open_flags_to_p3(open_flags),
                descriptor_flags_to_p3(flags),
                now,
            )
            .await
            .map_err(error_code_from_p3)
        }
        .await;
        match opened {
            Ok(opened) => Ok(Ok(self.table.push(opened)?)),
            Err(error) => Ok(Err(error)),
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
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (base, namespace) = target?;
            super::readlink_at(&namespace, &base, &path)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn remove_directory_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (base, namespace) = target?;
            super::remove_directory_at(&namespace, &base, &path)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn rename_at(
        &mut self,
        source_descriptor: Resource<FsDescriptor>,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = super::descriptor_pair_in(self, &source_descriptor, &destination_descriptor)
            .map_err(error_code_from_p3);
        let now = self.now_nanos();
        Ok(async {
            let (source_base, destination_base, namespace) = target?;
            super::rename_at(
                &namespace,
                &source_base,
                &source_path,
                &destination_base,
                &destination_path,
                now,
            )
            .await
            .map_err(error_code_from_p3)
        }
        .await)
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
        let target = self.p2_descriptor(&descriptor);
        let now = self.now_nanos();
        Ok(async {
            let (base, namespace) = target?;
            super::symlink_at(&namespace, &base, &old_path, &new_path, now)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
    }

    async fn unlink_file_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (base, namespace) = target?;
            super::unlink_file_at(&namespace, &base, &path)
                .await
                .map_err(error_code_from_p3)
        }
        .await)
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
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (descriptor, namespace) = target?;
            let metadata = super::stat_path(&namespace, &descriptor.path, crate::Follow::No)
                .await
                .map_err(error_code_from_p3)?;
            Ok(metadata_hash_from_p3(super::metadata_hash(&metadata)))
        }
        .await)
    }

    async fn metadata_hash_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
    ) -> Result<core::result::Result<p2fs::MetadataHashValue, p2fs::ErrorCode>> {
        let target = self.p2_descriptor(&descriptor);
        Ok(async {
            let (base, namespace) = target?;
            let metadata = super::stat_at(&namespace, &base, path_flags_to_p3(path_flags), &path)
                .await
                .map_err(error_code_from_p3)?;
            Ok(metadata_hash_from_p3(super::metadata_hash(&metadata)))
        }
        .await)
    }

    fn drop(&mut self, resource: Resource<FsDescriptor>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> filesystem_bindings::filesystem::types::HostDirectoryEntryStream
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

impl<CpuImpl, Net, HostFs> random_bindings::random::random::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        let len = super::random_len(len)?;
        let mut bytes = vec![0_u8; len];
        self.fill_secure_random(&mut bytes);
        Ok(bytes)
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(self.secure_random_u64())
    }
}

impl<CpuImpl, Net, HostFs> random_bindings::random::insecure::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(self.insecure_random_bytes(super::random_len(len)?))
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(self.insecure_random_u64())
    }
}

impl<CpuImpl, Net, HostFs> random_bindings::random::insecure_seed::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok(self.insecure_seed())
    }
}

fn delete_resource<R: 'static, CpuImpl, Net, HostFs>(
    store: &mut StoreData<CpuImpl, Net, HostFs>,
    resource: Resource<R>,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    store.table.delete(resource)?;
    Ok(())
}

impl<CpuImpl, Net, HostFs> p2net::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn network_error_code(
        &mut self,
        _: Resource<p2io_error::Error>,
    ) -> Result<Option<p2net::ErrorCode>> {
        Ok(None)
    }
}

impl<CpuImpl, Net, HostFs> p2net::HostNetwork for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<P2Network>) -> Result<()> {
        delete_resource(self, resource)
    }
}

impl<CpuImpl, Net, HostFs> sockets_bindings::sockets::instance_network::Host
    for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn instance_network(&mut self) -> Result<Resource<P2Network>> {
        self.table.push(P2Network).map_err(Into::into)
    }
}

impl<CpuImpl, Net, HostFs> p2udp::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, Net, HostFs> p2udp::HostUdpSocket for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2udp::ErrorCode::Unknown));
        };
        Ok(socket
            .start_bind_p2(&service, local_address)
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
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2udp::ErrorCode::Unknown));
        };
        let (incoming, outgoing) = match socket.open_p2_streams(&service, remote_address) {
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
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2udp::ErrorCode::Unknown));
        };
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_unicast_hop_limit(&service, value)
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

impl<CpuImpl, Net, HostFs> p2udp::HostIncomingDatagramStream for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn receive(
        &mut self,
        resource: Resource<p2udp::IncomingDatagramStream>,
        max_results: u64,
    ) -> Result<core::result::Result<Vec<p2udp::IncomingDatagram>, p2udp::ErrorCode>> {
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2udp::ErrorCode::Unknown));
        };
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
            match socket.receive_datagram(&service, remote_address, 0).await {
                Ok(datagram) => datagrams.push(p2udp::IncomingDatagram {
                    data: datagram.bytes.to_vec(),
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
        if let Some(service) = self.runtime_state.network_service() {
            self.table
                .get(&resource)?
                .socket
                .release_stream_handle(&service);
        }
        delete_resource(self, resource)
    }
}

impl<CpuImpl, Net, HostFs> p2udp::HostOutgoingDatagramStream for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2udp::ErrorCode::Unknown));
        };
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
                .send_datagram(&service, &datagram.data, remote_address, 0)
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
        if let Some(service) = self.runtime_state.network_service() {
            self.table
                .get(&resource)?
                .socket
                .release_stream_handle(&service);
        }
        delete_resource(self, resource)
    }
}

impl<CpuImpl, Net, HostFs> p2udp_create::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
            p2udp_create::IpAddressFamily::Ipv6 => WasiUdpSocketFamily::Ipv6,
        };
        if self.runtime_state.network_service().is_none() {
            return Ok(Err(p2udp_create::ErrorCode::Unknown));
        }
        let resource = self
            .table
            .push(UdpSocket::new(self.retirement.sender(), family))?;
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
                address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([
                    address.address.0,
                    address.address.1,
                    address.address.2,
                    address.address.3,
                ])),
                port: address.port,
            })
        }
        (WasiUdpSocketFamily::Ipv6, p2udp::IpSocketAddress::Ipv6(address)) => {
            let segments = [
                address.address.0,
                address.address.1,
                address.address.2,
                address.address.3,
                address.address.4,
                address.address.5,
                address.address.6,
                address.address.7,
            ];
            let mut octets = [0; 16];
            for (index, segment) in segments.into_iter().enumerate() {
                let [high, low] = segment.to_be_bytes();
                octets[index * 2] = high;
                octets[index * 2 + 1] = low;
            }
            Ok(WasiUdpSocketAddress {
                address: crate::NetworkIpAddress::Ipv6(Ipv6Address::new(octets)),
                port: address.port,
            })
        }
        (WasiUdpSocketFamily::Ipv4, p2udp::IpSocketAddress::Ipv6(_)) => {
            Err(WasiUdpSocketError::NotSupported)
        }
        (WasiUdpSocketFamily::Ipv6, p2udp::IpSocketAddress::Ipv4(_)) => {
            Err(WasiUdpSocketError::NotSupported)
        }
    }
}

fn format_p2_udp_socket_address(address: WasiUdpSocketAddress) -> p2udp::IpSocketAddress {
    match address.address {
        crate::NetworkIpAddress::Ipv4(ip) => {
            let [a, b, c, d] = ip.octets();
            p2udp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
                port: address.port,
                address: (a, b, c, d),
            })
        }
        crate::NetworkIpAddress::Ipv6(ip) => {
            let octets = ip.octets();
            let segments: [u16; 8] = core::array::from_fn(|index| {
                u16::from_be_bytes([octets[index * 2], octets[index * 2 + 1]])
            });
            p2udp::IpSocketAddress::Ipv6(p2net::Ipv6SocketAddress {
                port: address.port,
                flow_info: 0,
                address: (
                    segments[0],
                    segments[1],
                    segments[2],
                    segments[3],
                    segments[4],
                    segments[5],
                    segments[6],
                    segments[7],
                ),
                scope_id: 0,
            })
        }
    }
}

fn format_p2_udp_family(family: WasiUdpSocketFamily) -> p2udp::IpAddressFamily {
    match family {
        WasiUdpSocketFamily::Ipv4 => p2udp::IpAddressFamily::Ipv4,
        WasiUdpSocketFamily::Ipv6 => p2udp::IpAddressFamily::Ipv6,
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
                address: super::WasiTcpIpAddress::Ipv4(crate::Ipv4Address::new([
                    address.address.0,
                    address.address.1,
                    address.address.2,
                    address.address.3,
                ])),
                port: address.port,
            })
        }
        (WasiTcpSocketFamily::Ipv6, p2tcp::IpSocketAddress::Ipv6(address)) => {
            let segments = [
                address.address.0,
                address.address.1,
                address.address.2,
                address.address.3,
                address.address.4,
                address.address.5,
                address.address.6,
                address.address.7,
            ];
            let mut octets = [0; 16];
            for (index, segment) in segments.into_iter().enumerate() {
                let [high, low] = segment.to_be_bytes();
                octets[index * 2] = high;
                octets[index * 2 + 1] = low;
            }
            Ok(WasiTcpSocketAddress {
                address: super::WasiTcpIpAddress::Ipv6(helios_netstack::Ipv6Address::new(octets)),
                port: address.port,
            })
        }
        (WasiTcpSocketFamily::Ipv4, p2tcp::IpSocketAddress::Ipv6(_)) => {
            Err(p2tcp::ErrorCode::NotSupported)
        }
        (WasiTcpSocketFamily::Ipv6, p2tcp::IpSocketAddress::Ipv4(_)) => {
            Err(p2tcp::ErrorCode::NotSupported)
        }
    }
}

fn format_p2_tcp_socket_address(address: WasiTcpSocketAddress) -> p2tcp::IpSocketAddress {
    match address.address {
        super::WasiTcpIpAddress::Ipv4(ip) => {
            let [a, b, c, d] = ip.octets();
            p2tcp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
                port: address.port,
                address: (a, b, c, d),
            })
        }
        super::WasiTcpIpAddress::Ipv6(ip) => {
            let octets = ip.octets();
            let segment =
                |index: usize| u16::from_be_bytes([octets[index * 2], octets[index * 2 + 1]]);
            p2tcp::IpSocketAddress::Ipv6(p2net::Ipv6SocketAddress {
                port: address.port,
                flow_info: 0,
                address: (
                    segment(0),
                    segment(1),
                    segment(2),
                    segment(3),
                    segment(4),
                    segment(5),
                    segment(6),
                    segment(7),
                ),
                scope_id: 0,
            })
        }
    }
}

fn map_p2_tcp_family(
    family: p2tcp_create::IpAddressFamily,
) -> core::result::Result<WasiTcpSocketFamily, p2tcp_create::ErrorCode> {
    match family {
        p2tcp_create::IpAddressFamily::Ipv4 => Ok(WasiTcpSocketFamily::Ipv4),
        p2tcp_create::IpAddressFamily::Ipv6 => Ok(WasiTcpSocketFamily::Ipv6),
    }
}

fn format_p2_tcp_family(family: WasiTcpSocketFamily) -> p2tcp::IpAddressFamily {
    match family {
        WasiTcpSocketFamily::Ipv4 => p2tcp::IpAddressFamily::Ipv4,
        WasiTcpSocketFamily::Ipv6 => p2tcp::IpAddressFamily::Ipv6,
    }
}

fn map_p2_tcp_core_error(error: crate::TcpError) -> p2tcp::ErrorCode {
    match error.kind {
        crate::TcpErrorKind::UnresolvedHost => p2tcp::ErrorCode::NameUnresolvable,
        crate::TcpErrorKind::Timeout => p2tcp::ErrorCode::Timeout,
        crate::TcpErrorKind::ConnectionReset => p2tcp::ErrorCode::ConnectionReset,
        crate::TcpErrorKind::ConnectionAborted => p2tcp::ErrorCode::ConnectionAborted,
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

fn p2_record_kernel_profile<CpuImpl, Net, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, Net, HostFs>,
    cpu: &CpuImpl,
    phase: &'static str,
    started_ticks: u64,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if runtime_state.profiling_enabled() {
        runtime_state.record_profile_stack_parts(
            ProfileScope::Kernel,
            "kernel;preview2;",
            phase,
            cpu.now().ticks().saturating_sub(started_ticks),
        );
    }
}

/// The `input-stream`/`output-stream` pair a connected TCP socket hands
/// the guest.
///
/// The streams are the socket: no bridge task and no byte channel stands
/// between them and the connection. `write` queues onto the send path and
/// submits it on the guest task's own poll, `read` drains the socket's
/// receive queue directly, and `ready`/`subscribe` park that same task on
/// the owning shard's progress signal — see [`TcpSocketInputStream`] for
/// the concurrency contract. Two fewer tasks per connection is also what
/// makes teardown trivial: a parked `ready` lives inside the guest's own
/// task and dies with the store, which is the lifetime the bridge's
/// channel race had to re-create by hand (#184).
fn p2_tcp_stream_pair<CpuImpl, Net, HostFs>(
    store: &mut StoreData<CpuImpl, Net, HostFs>,
    socket_resource: &Resource<TcpSocket>,
    socket: TcpSocket,
) -> Result<(Resource<p2tcp::InputStream>, Resource<p2tcp::OutputStream>)>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started = store.cpu.now().ticks();
    // The streams run without a store, so each carries its own clone of
    // the concrete service rather than reading one back out of the
    // socket, which holds none.
    let Some(service) = store.runtime_state.network_service() else {
        return Err(wasmtime::Error::new(crate::ProgramExecError {
            kind: crate::ProgramExecErrorKind::Internal,
            detail: crate::ProgramExecErrorDetail::InternalInvariant,
        }));
    };
    let input = store.table.push_child(
        Box::new(TcpSocketInputStream::new(socket.clone(), service.clone())) as DynInputStream,
        socket_resource,
    )?;
    let output = store.table.push_child(
        Box::new(TcpSocketOutputStream::new(socket, service)) as DynOutputStream,
        socket_resource,
    )?;
    p2_record_kernel_profile(&store.runtime_state, &store.cpu, "tcp-stream-pair", started);
    Ok((input, output))
}

impl<CpuImpl, Net, HostFs> p2tcp::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, Net, HostFs> p2tcp::HostTcpSocket for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn start_bind(
        &mut self,
        socket: Resource<TcpSocket>,
        network: Resource<p2tcp::Network>,
        local_address: p2tcp::IpSocketAddress,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let _ = self.table.get(&network)?;
        let socket = self.table.get(&socket)?.clone();
        let local_address = match parse_p2_tcp_socket_address(local_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(error)),
        };
        if !has_wasi_network_rights(
            self.process_authority(),
            wasi_tcp_bind_rights(local_address.port),
        ) {
            return Ok(Err(p2tcp::ErrorCode::AccessDenied));
        }
        Ok(socket
            .bind_local(local_address)
            .map_err(map_p2_tcp_socket_error))
    }

    fn finish_bind(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .local_address()
            .map(drop)
            .map_err(map_p2_tcp_socket_error))
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
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2tcp::ErrorCode::Unknown));
        };
        let (inner, ready, local_port, hop_limit) = {
            let mut state = socket.inner.lock();
            if state.stream.is_some() || state.connect_in_progress || state.connect_result.is_some()
            {
                return Ok(Err(p2tcp::ErrorCode::InvalidState));
            }
            state.connect_in_progress = true;
            (
                socket.inner.clone(),
                socket.ready.clone(),
                state.local_address.map_or(0, |address| address.port),
                state.hop_limit,
            )
        };
        let profile_cpu = self.cpu.clone();
        let profile_runtime_state = self.runtime_state.clone();
        let spawned = self.spawner().try_spawn_detached({
            let inner = inner.clone();
            async move {
                let started = profile_cpu.now().ticks();
                let result = service
                    .tcp_connect_address(
                        remote_address.address.network_address(),
                        remote_address.port,
                        local_port,
                        hop_limit,
                        u64::MAX,
                    )
                    .await;
                p2_record_kernel_profile(
                    &profile_runtime_state,
                    &profile_cpu,
                    "tcp-connect",
                    started,
                );
                let mut state = inner.lock();
                state.connect_in_progress = false;
                state.connect_result = Some(match result {
                    Ok(stream) => {
                        state.stream = Some(stream.into_raw());
                        state.remote_address = Some(remote_address);
                        Ok(())
                    }
                    Err(error) => Err(error),
                });
                ready.notify_all();
            }
        });
        if let Err(error) = spawned {
            inner.lock().connect_in_progress = false;
            tracing::warn!(
                target: "helios_kernel::program",
                %error,
                "refused a tcp connect task: the executor's instance share is full"
            );
            return Ok(Err(p2tcp::ErrorCode::OutOfMemory));
        }
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
            let started = self.cpu.now().ticks();
            let mut state = socket.inner.lock();
            if state.connect_in_progress {
                p2_record_kernel_profile(
                    &self.runtime_state,
                    &self.cpu,
                    "tcp-finish-connect-would-block",
                    started,
                );
                return Ok(Err(p2tcp::ErrorCode::WouldBlock));
            }
            let Some(result) = state.connect_result.take() else {
                p2_record_kernel_profile(
                    &self.runtime_state,
                    &self.cpu,
                    "tcp-finish-connect-not-in-progress",
                    started,
                );
                return Ok(Err(p2tcp::ErrorCode::NotInProgress));
            };
            if let Err(error) = result {
                p2_record_kernel_profile(
                    &self.runtime_state,
                    &self.cpu,
                    "tcp-finish-connect-error",
                    started,
                );
                return Ok(Err(map_p2_tcp_core_error(error)));
            }
            p2_record_kernel_profile(
                &self.runtime_state,
                &self.cpu,
                "tcp-finish-connect",
                started,
            );
        }
        let (input, output) = p2_tcp_stream_pair(self, &socket_resource, socket)?;
        Ok(Ok((input, output)))
    }

    fn start_listen(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2tcp::ErrorCode::Unknown));
        };
        let (inner, ready, local_address, listen_backlog, hop_limit) = {
            let mut state = socket.inner.lock();
            if state.stream.is_some()
                || state.listener.is_some()
                || state.connect_in_progress
                || state.listen_in_progress
                || state.listen_result.is_some()
            {
                return Ok(Err(p2tcp::ErrorCode::InvalidState));
            }
            let local_address = state.local_address.unwrap_or(WasiTcpSocketAddress {
                address: state.family.unspecified_address(),
                port: 0,
            });
            if !has_wasi_network_rights(
                self.process_authority(),
                wasi_tcp_bind_rights(local_address.port),
            ) {
                return Ok(Err(p2tcp::ErrorCode::AccessDenied));
            }
            state.listen_in_progress = true;
            (
                socket.inner.clone(),
                socket.ready.clone(),
                local_address,
                state.listen_backlog,
                state.hop_limit,
            )
        };
        let spawned = self.spawner().try_spawn_detached({
            let inner = inner.clone();
            async move {
                let result = service
                    .tcp_listen(
                        local_address.address.network_address(),
                        local_address.port,
                        listen_backlog,
                        hop_limit,
                    )
                    .await
                    .map(|listener| crate::TcpListener {
                        listener: listener.listener.into_raw(),
                        local_port: listener.local_port,
                    });
                let mut state = inner.lock();
                state.listen_in_progress = false;
                state.listen_result = Some(result);
                ready.notify_all();
            }
        });
        if let Err(error) = spawned {
            inner.lock().listen_in_progress = false;
            tracing::warn!(
                target: "helios_kernel::program",
                %error,
                "refused a tcp listen task: the executor's instance share is full"
            );
            return Ok(Err(p2tcp::ErrorCode::OutOfMemory));
        }
        Ok(Ok(()))
    }

    fn finish_listen(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        let mut state = socket.inner.lock();
        if state.listen_in_progress {
            return Ok(Err(p2tcp::ErrorCode::WouldBlock));
        }
        let Some(result) = state.listen_result.take() else {
            return Ok(Err(p2tcp::ErrorCode::NotInProgress));
        };
        let listener = match result {
            Ok(listener) => listener,
            Err(error) => return Ok(Err(map_p2_tcp_core_error(error))),
        };
        state.listener = Some(listener.listener);
        let family = state.family;
        state.local_address.get_or_insert(WasiTcpSocketAddress {
            address: family.unspecified_address(),
            port: listener.local_port,
        });
        Ok(Ok(()))
    }

    fn accept(
        &mut self,
        socket_resource: Resource<TcpSocket>,
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
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2tcp::ErrorCode::Unknown));
        };
        let socket = self.table.get(&socket_resource)?.clone();
        let pending = {
            let mut state = socket.inner.lock();
            if let Some(result) = state.accept_result.take() {
                let accepted_socket = match result {
                    Ok(accepted_socket) => accepted_socket,
                    Err(error) => return Ok(Err(map_p2_tcp_core_error(error))),
                };
                drop(state);
                let accepted_resource = self.table.push_child(accepted_socket, &socket_resource)?;
                let accepted_socket = self.table.get(&accepted_resource)?.clone();
                let (input, output) =
                    p2_tcp_stream_pair(self, &accepted_resource, accepted_socket)?;
                return Ok(Ok((accepted_resource, input, output)));
            }
            if state.accept_in_progress {
                return Ok(Err(p2tcp::ErrorCode::WouldBlock));
            }
            let Some(listener) = state.listener else {
                return Ok(Err(p2tcp::ErrorCode::InvalidState));
            };
            let Some(pending) = PendingAccept::start(&socket, &mut state, service, listener) else {
                return Err(wasmtime::Error::new(crate::ProgramExecError {
                    kind: crate::ProgramExecErrorKind::Internal,
                    detail: crate::ProgramExecErrorDetail::InternalInvariant,
                }));
            };
            pending
        };
        if pending.spawn(self.spawner()).is_err() {
            return Ok(Err(p2tcp::ErrorCode::OutOfMemory));
        }
        Ok(Err(p2tcp::ErrorCode::WouldBlock))
    }

    fn local_address(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::IpSocketAddress, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .local_address()
            .map(format_p2_tcp_socket_address)
            .map_err(map_p2_tcp_socket_error))
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
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.is_listening())
    }

    fn address_family(&mut self, socket: Resource<TcpSocket>) -> Result<p2tcp::IpAddressFamily> {
        let socket = self.table.get(&socket)?.clone();
        Ok(format_p2_tcp_family(socket.family()))
    }

    fn set_listen_backlog_size(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_listen_backlog_size(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn keep_alive_enabled(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<bool, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.keep_alive_enabled().map_err(map_p2_tcp_socket_error))
    }

    fn set_keep_alive_enabled(
        &mut self,
        socket: Resource<TcpSocket>,
        value: bool,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_keep_alive_enabled(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn keep_alive_idle_time(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::Duration, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .keep_alive_idle_time()
            .map_err(map_p2_tcp_socket_error))
    }

    fn set_keep_alive_idle_time(
        &mut self,
        socket: Resource<TcpSocket>,
        value: p2tcp::Duration,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_keep_alive_idle_time(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn keep_alive_interval(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<p2tcp::Duration, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .keep_alive_interval()
            .map_err(map_p2_tcp_socket_error))
    }

    fn set_keep_alive_interval(
        &mut self,
        socket: Resource<TcpSocket>,
        value: p2tcp::Duration,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_keep_alive_interval(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn keep_alive_count(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u32, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.keep_alive_count().map_err(map_p2_tcp_socket_error))
    }

    fn set_keep_alive_count(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u32,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_keep_alive_count(value)
            .map_err(map_p2_tcp_socket_error))
    }

    fn hop_limit(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u8, p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.hop_limit().map_err(map_p2_tcp_socket_error))
    }

    fn set_hop_limit(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u8,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(p2tcp::ErrorCode::Unknown));
        };
        Ok(socket
            .set_hop_limit(&service, value)
            .map_err(map_p2_tcp_socket_error))
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
        socket: Resource<TcpSocket>,
        shutdown_type: p2tcp::ShutdownType,
    ) -> Result<core::result::Result<(), p2tcp::ErrorCode>> {
        match shutdown_type {
            p2tcp::ShutdownType::Receive => {
                let socket = self.table.get(&socket)?.clone();
                Ok(socket.shutdown_receive().map_err(map_p2_tcp_socket_error))
            }
            p2tcp::ShutdownType::Send => {
                let Some(service) = self.runtime_state.network_service() else {
                    return Ok(Err(p2tcp::ErrorCode::Unknown));
                };
                let socket = self.table.get(&socket)?.clone();
                let stream = match socket.shutdown_send_state() {
                    Ok(value) => value,
                    Err(error) => return Ok(Err(map_p2_tcp_socket_error(error))),
                };
                if let Err(error) = self.spawner().try_spawn_detached(async move {
                    let _ = service
                        .tcp_shutdown_send(crate::NetworkHandle::from_raw(stream))
                        .await;
                }) {
                    tracing::warn!(
                        target: "helios_kernel::program",
                        %error,
                        "refused a tcp shutdown task: the executor's instance share is full"
                    );
                    return Ok(Err(p2tcp::ErrorCode::OutOfMemory));
                }
                Ok(Ok(()))
            }
            p2tcp::ShutdownType::Both => {
                let Some(service) = self.runtime_state.network_service() else {
                    return Ok(Err(p2tcp::ErrorCode::Unknown));
                };
                let socket = self.table.get(&socket)?.clone();
                if let Err(error) = socket.shutdown_receive() {
                    return Ok(Err(map_p2_tcp_socket_error(error)));
                }
                let stream = match socket.shutdown_send_state() {
                    Ok(value) => value,
                    Err(error) => return Ok(Err(map_p2_tcp_socket_error(error))),
                };
                if let Err(error) = self.spawner().try_spawn_detached(async move {
                    let _ = service
                        .tcp_shutdown_send(crate::NetworkHandle::from_raw(stream))
                        .await;
                }) {
                    tracing::warn!(
                        target: "helios_kernel::program",
                        %error,
                        "refused a tcp shutdown task: the executor's instance share is full"
                    );
                    return Ok(Err(p2tcp::ErrorCode::OutOfMemory));
                }
                Ok(Ok(()))
            }
        }
    }

    fn drop(&mut self, resource: Resource<TcpSocket>) -> Result<()> {
        // Deleting the handle is the whole of it: the stream is owned by
        // `TcpSocketState`, which retires it when the last clone of this
        // socket goes away.
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> p2tcp_create::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
        if self.runtime_state.network_service().is_none() {
            return Ok(Err(p2tcp_create::ErrorCode::Unknown));
        }
        let resource = self
            .table
            .push(TcpSocket::new(self.retirement.sender(), family))?;
        Ok(Ok(resource))
    }
}

impl<CpuImpl, Net, HostFs> p2lookup::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
        if let Err(error) = self.spawner().try_spawn_detached(async move {
            task_stream.complete(service.dns_resolve(&name, u64::MAX).await);
        }) {
            tracing::warn!(
                target: "helios_kernel::program",
                %error,
                "refused a dns resolve task: the executor's instance share is full"
            );
            return Ok(Err(p2lookup::ErrorCode::TemporaryResolverFailure));
        }
        let resource = self.table.push(stream)?;
        Ok(Ok(resource))
    }
}

impl<CpuImpl, Net, HostFs> p2lookup::HostResolveAddressStream for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

fn format_p2_ip_address(address: crate::NetworkIpAddress) -> p2lookup::IpAddress {
    match address {
        crate::NetworkIpAddress::Ipv4(address) => {
            let [a, b, c, d] = address.octets();
            p2lookup::IpAddress::Ipv4((a, b, c, d))
        }
        crate::NetworkIpAddress::Ipv6(address) => p2lookup::IpAddress::Ipv6(
            crate::wasmtime_adapter::wasi::net::ipv6_address_groups(address),
        ),
    }
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

fn p2_stream_error(error: p2fs::ErrorCode) -> StreamError {
    StreamError::LastOperationFailed(wasmtime::Error::new(P2FileStreamError::Failed(error)))
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
    use alloc::string::String;

    use futures_lite::future::block_on;
    use wasmtime::component::Resource;
    use wasmtime_wasi_io::bytes::Bytes;
    use wasmtime_wasi_io::poll::Pollable;
    use wasmtime_wasi_io::streams::{DynOutputStream, InputStream, StreamError};

    use crate::wasmtime_adapter::component_host::service::test_store;

    use super::filesystem_bindings::filesystem::types::HostDescriptor;
    use super::{
        EmptyInputStream, PREVIEW2_WIT_PACKAGES, WasiTcpSocketFamily, WasiUdpSocketError,
        WasiUdpSocketFamily, format_p2_tcp_socket_address, format_p2_udp_family,
        format_p2_udp_socket_address, map_p2_tcp_core_error, map_p2_tcp_family,
        map_p2_tcp_socket_error, p2net, p2tcp, p2tcp_create, p2udp, parse_p2_tcp_socket_address,
        parse_p2_udp_socket_address,
    };
    use super::{FsDescriptor, FsNodeKind, p3fs};

    #[test]
    fn p2_empty_stdin_signals_eof_instead_of_starving_blocking_read() {
        let mut stream = EmptyInputStream;
        // `ready` must resolve without waiting; a closed stream has nothing
        // left to wait for.
        block_on(stream.ready());
        for size in [0_usize, 1, 4096] {
            match stream.read(size) {
                Err(StreamError::Closed) => {}
                Ok(bytes) => panic!(
                    "empty stdin returned {} readable bytes instead of closing; \
                     wasmtime-wasi-io traps after MAX_BLOCKING_ATTEMPTS empty reads",
                    bytes.len()
                ),
                Err(error) => panic!("empty stdin read failed unexpectedly: {error:?}"),
            }
        }
    }

    /// What `write-via-stream` and `append-via-stream` write lands in the
    /// filesystem of the store that opened the descriptor, where the
    /// next open, read or exec of the path finds it (#487).
    #[test]
    fn p2_file_output_stream_writes_into_the_opening_store_filesystem() {
        let mut store = test_store::store_data_with(|_| {});
        let root = FsDescriptor {
            path: String::from("/"),
            kind: FsNodeKind::Directory,
            flags: p3fs::DescriptorFlags::READ
                | p3fs::DescriptorFlags::WRITE
                | p3fs::DescriptorFlags::MUTATE_DIRECTORY,
            identity: None,
            mount: None,
        };
        let descriptor = block_on(crate::wasmtime_adapter::wasi::open_at(
            store.filesystem(),
            &root,
            p3fs::PathFlags::empty(),
            "copy",
            p3fs::OpenFlags::CREATE | p3fs::OpenFlags::TRUNCATE,
            p3fs::DescriptorFlags::READ | p3fs::DescriptorFlags::WRITE,
            1,
        ))
        .expect("creating a file at the root must succeed");
        let write = |store: &mut test_store::TestStoreData,
                     stream: Resource<DynOutputStream>,
                     bytes: &'static [u8]| {
            let stream = store
                .table
                .get_mut::<DynOutputStream>(&stream)
                .expect("the stream resource must be live");
            assert!(
                stream
                    .check_write()
                    .expect("a stream on a writable file must grant a permit")
                    >= bytes.len()
            );
            stream
                .write(Bytes::from_static(bytes))
                .expect("writing a file the store holds must succeed");
            stream
                .flush()
                .expect("flushing an embedded file must succeed");
            // The parked batch reaches the filesystem when the stream is
            // next ready, as a guest's `blocking-flush` waits for it.
            block_on(stream.ready());
        };

        let resource = store
            .table
            .push(descriptor.clone())
            .expect("the descriptor must fit in the table");
        let stream = HostDescriptor::write_via_stream(&mut store, resource, 0)
            .expect("write-via-stream must not trap")
            .expect("write-via-stream must open on a writable file");
        write(&mut store, stream, b"helios ");
        let resource = store
            .table
            .push(descriptor.clone())
            .expect("the descriptor must fit in the table");
        let stream = HostDescriptor::append_via_stream(&mut store, resource)
            .expect("append-via-stream must not trap")
            .expect("append-via-stream must open on a writable file");
        write(&mut store, stream, "日本語 한국어".as_bytes());

        let written = block_on(crate::wasmtime_adapter::wasi::read(
            store.filesystem(),
            &descriptor,
            0,
            64,
        ))
        .expect("the written file must be readable");
        assert_eq!(written.as_ref(), "helios 日本語 한국어".as_bytes());
    }

    #[test]
    fn preview2_wit_packages_use_expected_version() {
        for (package, wit) in PREVIEW2_WIT_PACKAGES {
            let expected = {
                let mut expected = String::from("package ");
                expected.push_str(package);
                expected.push_str("@0.2.12;");
                expected
            };
            assert!(
                wit.lines().any(|line| line.trim() == expected),
                "preview2 WIT package {package} does not declare @0.2.12"
            );
        }
    }

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
    fn p2_tcp_ipv6_addresses_roundtrip_while_family_mismatches_fail() {
        assert_eq!(
            map_p2_tcp_family(p2tcp_create::IpAddressFamily::Ipv6)
                .expect("IPv6 TCP family must be supported"),
            WasiTcpSocketFamily::Ipv6
        );
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

        let ipv6_address = p2tcp::IpSocketAddress::Ipv6(p2net::Ipv6SocketAddress {
            port: 443,
            flow_info: 0,
            address: (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
            scope_id: 0,
        });
        let parsed = parse_p2_tcp_socket_address(ipv6_address, WasiTcpSocketFamily::Ipv6)
            .expect("IPv6 address should parse for IPv6 socket");
        match format_p2_tcp_socket_address(parsed) {
            p2tcp::IpSocketAddress::Ipv6(address) => {
                assert_eq!(address.port, 443);
                assert_eq!(address.address, (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));
            }
            p2tcp::IpSocketAddress::Ipv4(_) => panic!("IPv6 TCP address formatted as IPv4"),
        }
        let mismatched_ipv4 = parse_p2_tcp_socket_address(
            p2tcp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
                port: 80,
                address: (127, 0, 0, 1),
            }),
            WasiTcpSocketFamily::Ipv6,
        )
        .expect_err("IPv4 address must not be accepted by IPv6 socket");
        assert_eq!(mismatched_ipv4, p2tcp::ErrorCode::NotSupported);
    }

    #[test]
    fn p2_udp_ipv6_addresses_roundtrip_while_family_mismatches_fail() {
        let ipv4 = parse_p2_udp_socket_address(
            p2udp::IpSocketAddress::Ipv4(p2net::Ipv4SocketAddress {
                port: 53,
                address: (127, 0, 0, 1),
            }),
            WasiUdpSocketFamily::Ipv4,
        )
        .expect("IPv4 address should parse for IPv4 socket");
        assert_eq!(ipv4.port, 53);
        assert_eq!(
            format_p2_udp_family(WasiUdpSocketFamily::Ipv6),
            p2udp::IpAddressFamily::Ipv6
        );

        let ipv6_address = p2udp::IpSocketAddress::Ipv6(p2net::Ipv6SocketAddress {
            port: 53,
            flow_info: 0,
            address: (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
            scope_id: 0,
        });
        let ipv6 = parse_p2_udp_socket_address(ipv6_address, WasiUdpSocketFamily::Ipv6)
            .expect("IPv6 address should parse for IPv6 socket");
        match format_p2_udp_socket_address(ipv6) {
            p2udp::IpSocketAddress::Ipv6(address) => {
                assert_eq!(address.port, 53);
                assert_eq!(address.address, (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));
            }
            p2udp::IpSocketAddress::Ipv4(_) => panic!("IPv6 UDP address formatted as IPv4"),
        }

        let mismatched_ipv6 = parse_p2_udp_socket_address(ipv6_address, WasiUdpSocketFamily::Ipv4)
            .expect_err("IPv6 address must not be accepted by IPv4 socket");
        assert!(matches!(mismatched_ipv6, WasiUdpSocketError::NotSupported));
    }
}
