extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::{ComponentNetworkService, EmbeddedBootFile, EmbeddedBootFs, HostFsErrorKind};
use bytes::{Bytes, BytesMut};
use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use spin::Mutex;
use wasmtime::component::{
    Access, Accessor, Component, Destination, FutureReader, HasSelf, Linker, Resource, Source,
    StreamConsumer, StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{self, Result};

use crate::component_host::{OutputStreamKind, StoreData};

pub(crate) type FsNodeKind = crate::ComponentFsNodeKind;
const FILE_READ_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_WASI_UDP_BUFFER_BYTES: u64 = 64 * 1024;
const DEFAULT_WASI_UDP_HOP_LIMIT: u8 = 64;
const MAX_WASI_UDP_DATAGRAM_BYTES: usize = u16::MAX as usize;

pub(crate) struct WasiImportSet {
    names: Vec<String>,
}

impl WasiImportSet {
    pub(crate) fn from_component(engine: &wasmtime::Engine, component: &Component) -> Self {
        Self {
            names: component
                .component_type()
                .imports(engine)
                .map(|(name, _)| name.to_owned())
                .collect(),
        }
    }

    pub(crate) fn has(&self, interface: &str, version_prefix: &str) -> bool {
        self.names.iter().any(|name| {
            name.strip_prefix(interface)
                .and_then(|suffix| suffix.strip_prefix('@'))
                .is_some_and(|version| version.starts_with(version_prefix))
        })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }
}

pub(crate) mod p2;

pub(crate) mod bindings;

use bindings as wasi;
use wasi::cli::types as cli_types;
use wasi::filesystem::types as fs_types;
use wasi::sockets::ip_name_lookup;
use wasi::sockets::types as socket_types;

pub(crate) fn add_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
    imports: &WasiImportSet,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if imports.has("wasi:clocks/monotonic-clock", "0.3") {
        wasi::clocks::monotonic_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:clocks/system-clock", "0.3") {
        wasi::clocks::system_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/environment", "0.3") {
        wasi::cli::environment::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/exit", "0.3") {
        wasi::cli::exit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            &Default::default(),
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdin", "0.3") {
        wasi::cli::stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdout", "0.3") {
        wasi::cli::stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stderr", "0.3") {
        wasi::cli::stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-input", "0.3") {
        wasi::cli::terminal_input::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-output", "0.3") {
        wasi::cli::terminal_output::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stdin", "0.3") {
        wasi::cli::terminal_stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stdout", "0.3") {
        wasi::cli::terminal_stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stderr", "0.3") {
        wasi::cli::terminal_stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/random", "0.3") {
        wasi::random::random::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/insecure", "0.3") {
        wasi::random::insecure::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/insecure-seed", "0.3") {
        wasi::random::insecure_seed::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:filesystem/types", "0.3") {
        wasi::filesystem::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:filesystem/preopens", "0.3") {
        wasi::filesystem::preopens::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/types", "0.3") {
        wasi::sockets::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/ip-name-lookup", "0.3") {
        wasi::sockets::ip_name_lookup::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    Ok(())
}

#[derive(Clone)]
struct FsNode {
    path: String,
    kind: FsNodeKind,
    contents: Bytes,
    inode: u64,
    modified_nanos: u64,
    readonly: bool,
}

#[derive(Clone)]
pub struct FsDescriptor {
    pub(crate) path: String,
    pub(crate) kind: FsNodeKind,
    pub(crate) flags: fs_types::DescriptorFlags,
}

pub struct TerminalInput;
pub struct TerminalOutput;
pub struct P2Network;
pub struct TcpSocket;
#[derive(Clone)]
pub struct UdpSocket {
    inner: Arc<Mutex<UdpSocketState>>,
}

pub struct P2IncomingDatagramStream {
    socket: UdpSocket,
    remote_address: Option<WasiUdpSocketAddress>,
}

pub struct P2OutgoingDatagramStream {
    socket: UdpSocket,
    remote_address: Option<WasiUdpSocketAddress>,
    check_send_permit_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WasiUdpSocketFamily {
    Ipv4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WasiUdpSocketAddress {
    address: crate::Ipv4Address,
    port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoundUdpSocket {
    socket: u64,
    local_port: u16,
}

struct UdpSocketState {
    service: crate::DynamicNetworkService,
    family: WasiUdpSocketFamily,
    bound: Option<BoundUdpSocket>,
    pending_bind: Option<BoundUdpSocket>,
    remote_address: Option<WasiUdpSocketAddress>,
    hop_limit: u8,
    receive_buffer_size: u64,
    send_buffer_size: u64,
    open_stream_handles: usize,
}

#[derive(Debug)]
enum WasiUdpSocketError {
    NotSupported,
    InvalidArgument,
    InvalidState,
    NotInProgress,
    AddressNotBindable,
    DatagramTooLarge,
    WouldBlock,
    Backend(crate::UdpError),
}

#[repr(transparent)]
pub struct TrappableError<T> {
    err: wasmtime::Error,
    _marker: PhantomData<T>,
}

pub type FsError = TrappableError<fs_types::ErrorCode>;

impl<T> TrappableError<T> {
    fn trap(err: impl Into<wasmtime::Error>) -> Self {
        Self {
            err: err.into(),
            _marker: PhantomData,
        }
    }

    fn downcast(self) -> Result<T>
    where
        T: core::error::Error + Send + Sync + 'static,
    {
        self.err.downcast()
    }
}

impl<T> From<T> for TrappableError<T>
where
    T: core::error::Error + Send + Sync + 'static,
{
    fn from(error: T) -> Self {
        Self::trap(error)
    }
}

impl<T> core::fmt::Debug for TrappableError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.err.fmt(f)
    }
}

impl<T> core::fmt::Display for TrappableError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.err.fmt(f)
    }
}

impl<T> core::error::Error for TrappableError<T> {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WasiUdpSocketDatagram {
    bytes: Vec<u8>,
    remote_address: WasiUdpSocketAddress,
}

impl UdpSocket {
    fn new(service: crate::DynamicNetworkService, family: WasiUdpSocketFamily) -> Self {
        Self {
            inner: Arc::new(Mutex::new(UdpSocketState {
                service,
                family,
                bound: None,
                pending_bind: None,
                remote_address: None,
                hop_limit: DEFAULT_WASI_UDP_HOP_LIMIT,
                receive_buffer_size: DEFAULT_WASI_UDP_BUFFER_BYTES,
                send_buffer_size: DEFAULT_WASI_UDP_BUFFER_BYTES,
                open_stream_handles: 0,
            })),
        }
    }

    fn family(&self) -> WasiUdpSocketFamily {
        self.inner.lock().family
    }

    fn local_address(&self) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
        let state = self.inner.lock();
        let Some(bound) = state.bound else {
            return Err(WasiUdpSocketError::InvalidState);
        };
        Ok(WasiUdpSocketAddress {
            address: crate::Ipv4Address::new([0, 0, 0, 0]),
            port: bound.local_port,
        })
    }

    fn remote_address(&self) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
        self.inner
            .lock()
            .remote_address
            .ok_or(WasiUdpSocketError::InvalidState)
    }

    fn unicast_hop_limit(&self) -> core::result::Result<u8, WasiUdpSocketError> {
        Ok(self.inner.lock().hop_limit)
    }

    fn set_unicast_hop_limit(&self, value: u8) -> core::result::Result<(), WasiUdpSocketError> {
        if value == 0 {
            return Err(WasiUdpSocketError::InvalidArgument);
        }
        self.inner.lock().hop_limit = value;
        Ok(())
    }

    fn receive_buffer_size(&self) -> core::result::Result<u64, WasiUdpSocketError> {
        Ok(self.inner.lock().receive_buffer_size)
    }

    fn set_receive_buffer_size(&self, value: u64) -> core::result::Result<(), WasiUdpSocketError> {
        if value == 0 {
            return Err(WasiUdpSocketError::InvalidArgument);
        }
        self.inner.lock().receive_buffer_size = value;
        Ok(())
    }

    fn send_buffer_size(&self) -> core::result::Result<u64, WasiUdpSocketError> {
        Ok(self.inner.lock().send_buffer_size)
    }

    fn set_send_buffer_size(&self, value: u64) -> core::result::Result<(), WasiUdpSocketError> {
        if value == 0 {
            return Err(WasiUdpSocketError::InvalidArgument);
        }
        self.inner.lock().send_buffer_size = value;
        Ok(())
    }

    async fn bind(
        &self,
        local_address: WasiUdpSocketAddress,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        validate_udp_local_address(self.family(), local_address)?;
        self.bind_backend(local_address.port, false).await
    }

    async fn start_bind_p2(
        &self,
        local_address: WasiUdpSocketAddress,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        validate_udp_local_address(self.family(), local_address)?;
        self.bind_backend(local_address.port, true).await
    }

    fn finish_bind_p2(&self) -> core::result::Result<(), WasiUdpSocketError> {
        let mut state = self.inner.lock();
        let Some(bound) = state.pending_bind.take() else {
            return Err(WasiUdpSocketError::NotInProgress);
        };
        assert!(
            state.bound.is_none(),
            "udp socket completed a p2 bind while already bound"
        );
        state.bound = Some(bound);
        Ok(())
    }

    async fn connect(
        &self,
        remote_address: WasiUdpSocketAddress,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        validate_udp_remote_address(self.family(), remote_address)?;
        let _ = self.ensure_bound(0).await?;
        self.inner.lock().remote_address = Some(remote_address);
        Ok(())
    }

    fn disconnect(&self) -> core::result::Result<(), WasiUdpSocketError> {
        let mut state = self.inner.lock();
        if state.remote_address.is_none() {
            return Err(WasiUdpSocketError::InvalidState);
        }
        state.remote_address = None;
        Ok(())
    }

    fn open_p2_streams(
        &self,
        remote_address: Option<WasiUdpSocketAddress>,
    ) -> core::result::Result<
        (P2IncomingDatagramStream, P2OutgoingDatagramStream),
        WasiUdpSocketError,
    > {
        if let Some(remote_address) = remote_address {
            validate_udp_remote_address(self.family(), remote_address)?;
        }
        let mut state = self.inner.lock();
        if state.pending_bind.is_some() || state.bound.is_none() {
            return Err(WasiUdpSocketError::InvalidState);
        }
        if state.open_stream_handles != 0 {
            return Err(WasiUdpSocketError::InvalidState);
        }
        state.remote_address = remote_address;
        state.open_stream_handles = 2;
        let incoming = P2IncomingDatagramStream {
            socket: self.clone(),
            remote_address,
        };
        let outgoing = P2OutgoingDatagramStream {
            socket: self.clone(),
            remote_address,
            check_send_permit_count: 0,
        };
        Ok((incoming, outgoing))
    }

    fn release_stream_handle(&self) {
        let mut state = self.inner.lock();
        if state.open_stream_handles == 0 {
            return;
        }
        state.open_stream_handles -= 1;
    }

    async fn send_datagram(
        &self,
        bytes: &[u8],
        remote_address: Option<WasiUdpSocketAddress>,
        timeout_nanos: u64,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        if bytes.len() > MAX_WASI_UDP_DATAGRAM_BYTES {
            return Err(WasiUdpSocketError::DatagramTooLarge);
        }
        let target = {
            let state = self.inner.lock();
            resolve_udp_send_target(&state, remote_address)?
        };
        let bound = self.ensure_bound(0).await?;
        let service = self.inner.lock().service.clone();
        let host = format_ipv4_address(target.address);
        let written = service
            .udp_send(bound.socket, &host, target.port, bytes, timeout_nanos)
            .await
            .map_err(WasiUdpSocketError::Backend)?;
        assert!(
            usize::try_from(written).ok() == Some(bytes.len()),
            "kernel udp service reported a partial datagram write"
        );
        Ok(())
    }

    async fn receive_datagram(
        &self,
        remote_address: Option<WasiUdpSocketAddress>,
        timeout_nanos: u64,
    ) -> core::result::Result<WasiUdpSocketDatagram, WasiUdpSocketError> {
        let filter = {
            let state = self.inner.lock();
            if state.pending_bind.is_some() || state.bound.is_none() {
                return Err(WasiUdpSocketError::InvalidState);
            }
            remote_address.or(state.remote_address)
        };
        let bound = self
            .inner
            .lock()
            .bound
            .ok_or(WasiUdpSocketError::InvalidState)?;
        let service = self.inner.lock().service.clone();
        loop {
            let datagram = service
                .udp_receive(
                    bound.socket,
                    MAX_WASI_UDP_DATAGRAM_BYTES as u32,
                    timeout_nanos,
                )
                .await
                .map_err(|error| {
                    if timeout_nanos == 0 && matches!(error.kind, crate::UdpErrorKind::Timeout) {
                        WasiUdpSocketError::WouldBlock
                    } else {
                        WasiUdpSocketError::Backend(error)
                    }
                })?
                .ok_or(WasiUdpSocketError::InvalidState)?;
            let remote = WasiUdpSocketAddress {
                address: datagram.address,
                port: datagram.port,
            };
            if filter.is_some_and(|expected| expected != remote) {
                continue;
            }
            return Ok(WasiUdpSocketDatagram {
                bytes: datagram.bytes,
                remote_address: remote,
            });
        }
    }

    async fn bind_backend(
        &self,
        local_port: u16,
        pending_bind: bool,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        let service = {
            let state = self.inner.lock();
            if !matches!(state.family, WasiUdpSocketFamily::Ipv4) {
                return Err(WasiUdpSocketError::NotSupported);
            }
            if state.pending_bind.is_some() || state.bound.is_some() {
                return Err(WasiUdpSocketError::InvalidState);
            }
            state.service.clone()
        };
        let binding = service
            .udp_bind(local_port)
            .await
            .map_err(WasiUdpSocketError::Backend)?;
        let bound = BoundUdpSocket {
            socket: binding.socket,
            local_port: binding.local_port,
        };
        let mut state = self.inner.lock();
        assert!(
            state.pending_bind.is_none() && state.bound.is_none(),
            "udp bind raced with a concurrent state transition"
        );
        if pending_bind {
            state.pending_bind = Some(bound);
        } else {
            state.bound = Some(bound);
        }
        Ok(())
    }

    async fn ensure_bound(
        &self,
        local_port: u16,
    ) -> core::result::Result<BoundUdpSocket, WasiUdpSocketError> {
        {
            let state = self.inner.lock();
            if let Some(bound) = state.bound {
                return Ok(bound);
            }
            if state.pending_bind.is_some() {
                return Err(WasiUdpSocketError::InvalidState);
            }
        }
        self.bind_backend(local_port, false).await?;
        self.inner
            .lock()
            .bound
            .ok_or(WasiUdpSocketError::InvalidState)
    }
}

fn resolve_udp_send_target(
    state: &UdpSocketState,
    remote_address: Option<WasiUdpSocketAddress>,
) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
    match (state.remote_address, remote_address) {
        (Some(connected), None) => Ok(connected),
        (Some(connected), Some(provided)) if connected == provided => Ok(connected),
        (None, Some(provided)) => {
            validate_udp_remote_address(state.family, provided)?;
            Ok(provided)
        }
        _ => Err(WasiUdpSocketError::InvalidArgument),
    }
}

fn validate_udp_local_address(
    family: WasiUdpSocketFamily,
    local_address: WasiUdpSocketAddress,
) -> core::result::Result<(), WasiUdpSocketError> {
    if !matches!(family, WasiUdpSocketFamily::Ipv4) {
        return Err(WasiUdpSocketError::NotSupported);
    }
    if local_address.address.octets() != [0, 0, 0, 0] {
        return Err(WasiUdpSocketError::AddressNotBindable);
    }
    Ok(())
}

fn validate_udp_remote_address(
    family: WasiUdpSocketFamily,
    remote_address: WasiUdpSocketAddress,
) -> core::result::Result<(), WasiUdpSocketError> {
    if !matches!(family, WasiUdpSocketFamily::Ipv4) {
        return Err(WasiUdpSocketError::NotSupported);
    }
    if remote_address.address.octets() == [0, 0, 0, 0] || remote_address.port == 0 {
        return Err(WasiUdpSocketError::InvalidArgument);
    }
    Ok(())
}

fn format_ipv4_address(address: crate::Ipv4Address) -> String {
    let [a, b, c, d] = address.octets();
    format!("{a}.{b}.{c}.{d}")
}

pub(crate) struct DebugFileSystem<State, HostFsService> {
    nodes: Vec<FsNode>,
    next_inode: u64,
    runtime_state: State,
    _host_fs: PhantomData<HostFsService>,
}

struct SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    stream: OutputStreamKind,
    result: Option<oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>>,
}

impl<T, CpuImpl, HostFs> SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        result: oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>,
        stream: OutputStreamKind,
    ) -> Self {
        Self {
            getter,
            stream,
            result: Some(result),
        }
    }

    fn complete(&mut self, result: core::result::Result<(), cli_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T, CpuImpl, HostFs> Drop for SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamConsumer<T> for SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;
        let consumer = self.as_ref().get_ref();
        let getter = consumer.getter;
        getter(store.data_mut()).write_output(consumer.stream, &bytes);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

enum FileWriteMode {
    At(usize),
    Append,
}

struct FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    descriptor: FsDescriptor,
    mode: FileWriteMode,
    result: Option<oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>>,
}

struct FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    descriptor: FsDescriptor,
    offset: u64,
    chunk_bytes: usize,
    result: Option<oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>>,
}

impl<T, CpuImpl, HostFs> FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        offset: u64,
        chunk_bytes: usize,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            offset,
            chunk_bytes,
            result: Some(result),
        }
    }

    fn complete(&mut self, result: core::result::Result<(), fs_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T, CpuImpl, HostFs> Drop for FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamProducer<T> for FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            self.complete(Ok(()));
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        let getter = self.getter;
        let store_data = getter(store.data_mut());
        match store_data.filesystem().read_file_chunk(
            &self.descriptor,
            self.offset,
            self.chunk_bytes,
        ) {
            Ok(bytes) if bytes.is_empty() => {
                self.complete(Ok(()));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Ok(bytes) => {
                self.offset = self
                    .offset
                    .checked_add(
                        u64::try_from(bytes.len()).expect("read chunk size overflowed u64"),
                    )
                    .ok_or_else(|| wasmtime::Error::msg("file read offset overflowed u64"))?;
                destination.set_buffer(bytes.into());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(error) => {
                self.complete(Err(error));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

impl<T, CpuImpl, HostFs> FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new_at(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        offset: usize,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            mode: FileWriteMode::At(offset),
            result: Some(result),
        }
    }

    fn new_append(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            mode: FileWriteMode::Append,
            result: Some(result),
        }
    }

    fn complete(&mut self, result: core::result::Result<(), fs_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T, CpuImpl, HostFs> Drop for FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamConsumer<T> for FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;

        let store_data = (self.getter)(store.data_mut());
        let descriptor = self.descriptor.clone();
        let now_nanos = store_data.now_nanos();
        let write_result = match &mut self.mode {
            FileWriteMode::At(offset) => {
                let result =
                    store_data
                        .filesystem_mut()
                        .write_at(&descriptor, *offset, &bytes, now_nanos);
                if result.is_ok() {
                    *offset = offset
                        .checked_add(bytes.len())
                        .expect("file write offset overflowed usize");
                }
                result
            }
            FileWriteMode::Append => {
                store_data
                    .filesystem_mut()
                    .append(&descriptor, &bytes, now_nanos)
            }
        };

        match write_result {
            Ok(()) => Poll::Ready(Ok(StreamResult::Completed)),
            Err(error) => {
                self.complete(Err(error));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

impl<State, HostFsService> DebugFileSystem<State, HostFsService>
where
    State: crate::ComponentHostFilesystemState<HostFsService>,
    HostFsService: crate::HostFileSystem,
{
    pub(crate) fn new(runtime_state: State) -> Self {
        let mut filesystem = Self {
            nodes: vec![FsNode {
                path: String::from("/"),
                kind: FsNodeKind::Directory,
                contents: Bytes::new(),
                inode: 1,
                modified_nanos: 0,
                readonly: false,
            }],
            next_inode: 2,
            runtime_state,
            _host_fs: PhantomData,
        };

        if let Some(bootfs) = filesystem.runtime_state.bootfs() {
            filesystem.seed_bootfs(bootfs);
        }

        filesystem
    }

    fn seed_bootfs(&mut self, image: EmbeddedBootFs) {
        for file in image.files() {
            self.insert_bootfs_file(file);
        }
    }

    fn insert_bootfs_file(&mut self, file: &EmbeddedBootFile) {
        let absolute = format!("/{}", file.path());
        let mut current = String::from("/");

        for segment in file.path().split('/').filter(|segment| !segment.is_empty()) {
            let next = if current == "/" {
                format!("/{segment}")
            } else {
                format!("{current}/{segment}")
            };

            if next == absolute {
                break;
            }

            self.ensure_directory(&next, true);
            current = next;
        }

        if self.get_node(&absolute).is_ok() {
            return;
        }

        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: absolute,
            kind: FsNodeKind::File,
            contents: Bytes::from_static(file.contents()),
            inode,
            modified_nanos: 0,
            readonly: true,
        });
    }

    /// Cache host file content in the embedded FS so that subsequent stream
    /// reads can proceed synchronously without async 9p I/O.
    pub(crate) fn seed_host_file_content(&mut self, path: &str, content: Vec<u8>) {
        if let Some(node) = self.nodes.iter_mut().find(|n| n.path == path) {
            node.contents = Bytes::from(content);
            return;
        }
        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: path.to_owned(),
            kind: FsNodeKind::File,
            contents: Bytes::from(content),
            inode,
            modified_nanos: 0,
            readonly: true,
        });
    }

    /// Drop any cached embedded nodes mirroring a host subtree. Called after
    /// mutating operations (remove, rename) so stale placeholder nodes do
    /// not linger.
    pub(crate) fn invalidate_host_subtree(&mut self, path: &str) {
        let prefix = crate::directory_prefix(path);
        self.nodes
            .retain(|node| node.path != path && !node.path.starts_with(&prefix));
    }

    /// Seed direct children of a host directory into the embedded FS so that
    /// later `read_directory` calls see the same entries without awaiting
    /// host I/O inside a sync stream context.
    pub(crate) fn seed_host_directory_entries(
        &mut self,
        path: &str,
        entries: Vec<crate::HostDirEntry>,
    ) {
        self.ensure_directory(path, true);
        let prefix = crate::directory_prefix(path);
        for entry in entries {
            let child_path = format!("{prefix}{}", entry.name);
            if self.nodes.iter().any(|node| node.path == child_path) {
                continue;
            }
            let inode = self.allocate_inode();
            self.nodes.push(FsNode {
                path: child_path,
                kind: if entry.is_directory {
                    FsNodeKind::Directory
                } else {
                    FsNodeKind::File
                },
                contents: Bytes::new(),
                inode,
                modified_nanos: 0,
                readonly: true,
            });
        }
    }

    fn ensure_directory(&mut self, path: &str, readonly: bool) {
        if path == "/" {
            return;
        }

        if let Some(existing) = self.nodes.iter_mut().find(|node| node.path == path) {
            assert!(
                existing.kind == FsNodeKind::Directory,
                "bootfs directory path {} collided with an existing file",
                path
            );
            existing.readonly |= readonly;
            return;
        }

        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: path.to_owned(),
            kind: FsNodeKind::Directory,
            contents: Bytes::new(),
            inode,
            modified_nanos: 0,
            readonly,
        });
    }

    fn allocate_inode(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }

    fn host_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        crate::guest_host_share_path(path)
    }

    fn host_service(&self) -> core::result::Result<HostFsService, fs_types::ErrorCode> {
        self.runtime_state
            .host_filesystem_service()
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    pub(crate) fn root_descriptor(&self) -> FsDescriptor {
        FsDescriptor {
            path: String::from("/"),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
        }
    }

    fn get_node(&self, path: &str) -> core::result::Result<&FsNode, fs_types::ErrorCode> {
        self.nodes
            .iter()
            .find(|node| node.path == path)
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    fn get_node_mut(
        &mut self,
        path: &str,
    ) -> core::result::Result<&mut FsNode, fs_types::ErrorCode> {
        self.nodes
            .iter_mut()
            .find(|node| node.path == path)
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    pub(crate) fn stat(
        &self,
        path: &str,
    ) -> core::result::Result<fs_types::DescriptorStat, fs_types::ErrorCode> {
        // Host paths are normally handled asynchronously in the WASI
        // trait impl, but fall through to the embedded view when the
        // node has already been seeded (open_at eagerly mirrors the
        // host file/directory). If neither exists, return NoEntry
        // instead of panicking so programs see a proper filesystem
        // error.
        let node = self.get_node(path)?;
        let size = match node.kind {
            FsNodeKind::Directory => 0,
            FsNodeKind::File => node.contents.len() as u64,
        };
        let timestamp = system_time_from_nanos(node.modified_nanos);
        Ok(fs_types::DescriptorStat {
            type_: match node.kind {
                FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                FsNodeKind::File => fs_types::DescriptorType::RegularFile,
            },
            link_count: 1,
            size,
            data_access_timestamp: Some(timestamp),
            data_modification_timestamp: Some(timestamp),
            status_change_timestamp: Some(timestamp),
        })
    }

    pub(crate) fn metadata_hash(
        &self,
        path: &str,
    ) -> core::result::Result<fs_types::MetadataHashValue, fs_types::ErrorCode> {
        // Host paths are normally handled asynchronously in the WASI
        // trait impl, but fall through to the embedded view when the
        // node has already been seeded.
        let node = self.get_node(path)?;
        let size = match node.kind {
            FsNodeKind::Directory => 0,
            FsNodeKind::File => node.contents.len() as u64,
        };
        Ok(fs_types::MetadataHashValue {
            lower: node.inode,
            upper: node.modified_nanos ^ size,
        })
    }

    pub(crate) fn read_directory(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<fs_types::DirectoryEntry>, fs_types::ErrorCode> {
        // Host directories are eagerly seeded into the embedded FS by
        // `open_at`, so reading them here walks the same embedded node
        // list as guest-owned directories. If nothing was seeded, just
        // report NoEntry.
        let node = self.get_node(path)?;
        if node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let prefix = crate::directory_prefix(path);
        let mut entries = Vec::new();
        for child in &self.nodes {
            if child.path == path {
                continue;
            }
            let Some(remainder) = child.path.strip_prefix(&prefix) else {
                continue;
            };
            if remainder.is_empty() {
                continue;
            }
            if let Some((name, _)) = remainder.split_once('/') {
                if entries
                    .iter()
                    .any(|entry: &fs_types::DirectoryEntry| entry.name == name)
                {
                    continue;
                }
                entries.push(fs_types::DirectoryEntry {
                    type_: fs_types::DescriptorType::Directory,
                    name: name.to_string(),
                });
                continue;
            }
            entries.push(fs_types::DirectoryEntry {
                type_: match child.kind {
                    FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                    FsNodeKind::File => fs_types::DescriptorType::RegularFile,
                },
                name: remainder.to_string(),
            });
        }

        if path == "/" && self.runtime_state.host_filesystem_service().is_some() {
            let has_host_mount = entries.iter().any(|entry| entry.name == "host");
            if !has_host_mount {
                entries.push(fs_types::DirectoryEntry {
                    type_: fs_types::DescriptorType::Directory,
                    name: String::from("host"),
                });
            }
        }

        entries.sort_by(|left, right| {
            is_dir_first(left.type_.clone())
                .cmp(&is_dir_first(right.type_.clone()))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(entries)
    }

    pub(crate) fn read_file_chunk(
        &self,
        descriptor: &FsDescriptor,
        offset: u64,
        max_bytes: usize,
    ) -> core::result::Result<Vec<u8>, fs_types::ErrorCode> {
        // Host file contents are eagerly cached during `open_at`, so
        // stream reads always fall through to the embedded node list.
        // If the node is missing, return NoEntry.
        let node = self.get_node(&descriptor.path)?;
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        if !descriptor.flags.contains(fs_types::DescriptorFlags::READ) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let offset: usize = offset
            .try_into()
            .map_err(|_| fs_types::ErrorCode::Overflow)?;
        let contents = node.contents.as_ref();
        if offset >= contents.len() {
            return Ok(Vec::new());
        }
        let end = offset
            .checked_add(max_bytes)
            .map(|value| value.min(contents.len()))
            .ok_or(fs_types::ErrorCode::Overflow)?;
        Ok(contents[offset..end].to_vec())
    }

    pub(crate) fn read_program_file_bytes(
        &self,
        path: &str,
    ) -> core::result::Result<Bytes, fs_types::ErrorCode> {
        let node = self.get_node(path)?;
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        Ok(node.contents.clone())
    }

    pub(crate) fn write_program_file(
        &mut self,
        path: &str,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if self.host_path(path).is_some() {
            return Err(fs_types::ErrorCode::Unsupported);
        }

        match self.get_node_mut(path) {
            Ok(node) => {
                if node.kind != FsNodeKind::File {
                    return Err(fs_types::ErrorCode::IsDirectory);
                }
                if node.readonly {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                node.contents = Bytes::copy_from_slice(bytes);
                node.modified_nanos = now_nanos;
                Ok(())
            }
            Err(fs_types::ErrorCode::NoEntry) => {
                let parent = crate::parent_path(path);
                let parent_node = self.get_node(parent)?;
                if parent_node.kind != FsNodeKind::Directory {
                    return Err(fs_types::ErrorCode::NotDirectory);
                }
                if parent_node.readonly {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                let inode = self.allocate_inode();
                self.nodes.push(FsNode {
                    path: path.to_owned(),
                    kind: FsNodeKind::File,
                    contents: Bytes::copy_from_slice(bytes),
                    inode,
                    modified_nanos: now_nanos,
                    readonly: false,
                });
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn is_readonly_path(
        &self,
        path: &str,
    ) -> core::result::Result<bool, fs_types::ErrorCode> {
        Ok(self.get_node(path)?.readonly)
    }

    pub(crate) fn write_at(
        &mut self,
        descriptor: &FsDescriptor,
        offset: usize,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        // Host-backed writes would require an async flush path; stream
        // consumers run in sync poll contexts, so we must not block here.
        // Writing to host files is not yet supported; callers see a clean
        // error rather than a hang.
        if self.host_path(&descriptor.path).is_some() {
            return Err(fs_types::ErrorCode::Unsupported);
        }

        let node = self.get_node_mut(&descriptor.path)?;
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let end = offset
            .checked_add(bytes.len())
            .ok_or(fs_types::ErrorCode::Overflow)?;
        let mut contents = BytesMut::from(&node.contents[..]);
        if contents.len() < offset {
            contents.resize(offset, 0);
        }
        if contents.len() < end {
            contents.resize(end, 0);
        }
        contents[offset..end].copy_from_slice(bytes);
        node.contents = contents.freeze();
        node.modified_nanos = now_nanos;
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        descriptor: &FsDescriptor,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        // Host-backed appends would require async I/O in a sync stream
        // consumer context; not supported without an async flush path.
        if self.host_path(&descriptor.path).is_some() {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        let offset = self.get_node(&descriptor.path)?.contents.len();
        self.write_at(descriptor, offset, bytes, now_nanos)
    }

    pub(crate) fn open_at(
        &mut self,
        base: &FsDescriptor,
        path_flags: fs_types::PathFlags,
        path: &str,
        open_flags: fs_types::OpenFlags,
        descriptor_flags: fs_types::DescriptorFlags,
        now_nanos: u64,
    ) -> core::result::Result<FsDescriptor, fs_types::ErrorCode> {
        // We deliberately ignore `path_flags::SYMLINK_FOLLOW`: our guest
        // filesystem does not support symlinks at all, so following them
        // is a no-op. POSIX `open()` defaults to following symlinks, so
        // rejecting the flag would break the vast majority of programs.
        let _ = path_flags;
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        // Host-backed paths are handled in the async WASI trait impl;
        // this embedded path only handles paths that already have a
        // node (either genuinely local or previously seeded from the
        // host mount).
        if let Ok(existing) = self.get_node_mut(&absolute) {
            if open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
                && open_flags.contains(fs_types::OpenFlags::CREATE)
            {
                return Err(fs_types::ErrorCode::Exist);
            }
            if open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                && existing.kind != FsNodeKind::Directory
            {
                return Err(fs_types::ErrorCode::NotDirectory);
            }
            if !open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                && existing.kind == FsNodeKind::Directory
            {
                return Err(fs_types::ErrorCode::IsDirectory);
            }
            if open_flags.contains(fs_types::OpenFlags::TRUNCATE) {
                if existing.kind != FsNodeKind::File {
                    return Err(fs_types::ErrorCode::IsDirectory);
                }
                if existing.readonly {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                if !base
                    .flags
                    .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                existing.contents = Bytes::new();
                existing.modified_nanos = now_nanos;
            }
            return Ok(FsDescriptor {
                path: absolute,
                kind: existing.kind,
                flags: descriptor_flags,
            });
        }

        if !open_flags.contains(fs_types::OpenFlags::CREATE) {
            return Err(fs_types::ErrorCode::NoEntry);
        }
        if open_flags.contains(fs_types::OpenFlags::DIRECTORY) {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        if self.get_node(&base.path)?.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let parent = crate::parent_path(&absolute);
        let parent_node = self.get_node(parent)?;
        if parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let inode = self.allocate_inode();
        let node = FsNode {
            path: absolute.clone(),
            kind: FsNodeKind::File,
            contents: Bytes::new(),
            inode,
            modified_nanos: now_nanos,
            readonly: false,
        };
        self.nodes.push(node);
        Ok(FsDescriptor {
            path: absolute,
            kind: FsNodeKind::File,
            flags: descriptor_flags,
        })
    }

    pub(crate) fn remove_directory_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        if absolute == "/" {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        let node = self.get_node(&absolute)?;
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        let prefix = crate::directory_prefix(&absolute);
        if self
            .nodes
            .iter()
            .any(|child| child.path != absolute && child.path.starts_with(&prefix))
        {
            return Err(fs_types::ErrorCode::NotEmpty);
        }
        self.nodes.retain(|node| node.path != absolute);
        Ok(())
    }

    pub(crate) fn create_directory_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        if self.get_node(&base.path)?.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        if absolute == "/" {
            return Err(fs_types::ErrorCode::Exist);
        }
        if self.get_node(&absolute).is_ok() {
            return Err(fs_types::ErrorCode::Exist);
        }

        let parent = crate::parent_path(&absolute);
        let parent_node = self.get_node(parent)?;
        if parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: absolute,
            kind: FsNodeKind::Directory,
            contents: Bytes::new(),
            inode,
            modified_nanos: now_nanos,
            readonly: false,
        });
        Ok(())
    }

    pub(crate) fn unlink_file_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        let node = self.get_node(&absolute)?;
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        self.nodes.retain(|node| node.path != absolute);
        Ok(())
    }

    pub(crate) fn rename_at(
        &mut self,
        source_base: &FsDescriptor,
        source_path: &str,
        destination_base: &FsDescriptor,
        destination_path: &str,
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if source_base.kind != FsNodeKind::Directory
            || destination_base.kind != FsNodeKind::Directory
        {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if !source_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            || !destination_base
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let source_absolute = crate::resolve_child_path(&source_base.path, source_path)
            .map_err(map_component_fs_path_error)?;
        let destination_absolute =
            crate::resolve_child_path(&destination_base.path, destination_path)
                .map_err(map_component_fs_path_error)?;

        if self.get_node(&source_base.path)?.readonly
            || self.get_node(&destination_base.path)?.readonly
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if source_absolute == "/" || destination_absolute == "/" {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        if source_absolute == destination_absolute {
            return Ok(());
        }

        let source_node = self.get_node(&source_absolute)?.clone();
        if source_node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if self.get_node(&destination_absolute).is_ok() {
            return Err(fs_types::ErrorCode::Exist);
        }

        let destination_parent = crate::parent_path(&destination_absolute);
        let destination_parent_node = self.get_node(destination_parent)?;
        if destination_parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if destination_parent_node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        if source_node.kind == FsNodeKind::Directory {
            let source_prefix = crate::directory_prefix(&source_absolute);
            if destination_absolute == source_absolute
                || destination_absolute.starts_with(&source_prefix)
            {
                return Err(fs_types::ErrorCode::NotPermitted);
            }
        }

        let source_prefix = crate::directory_prefix(&source_absolute);
        for node in &mut self.nodes {
            if node.path == source_absolute {
                node.path = destination_absolute.clone();
                node.modified_nanos = now_nanos;
                continue;
            }

            if source_node.kind == FsNodeKind::Directory && node.path.starts_with(&source_prefix) {
                node.path = format!(
                    "{}{suffix}",
                    destination_absolute,
                    suffix = &node.path[source_absolute.len()..]
                );
                node.modified_nanos = now_nanos;
            }
        }

        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::clocks::monotonic_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<wasi::clocks::monotonic_clock::Mark> {
        Ok(self.now_nanos())
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl<CpuImpl, HostFs> wasi::clocks::monotonic_clock::HostWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn wait_until<T: Send>(
        accessor: &Accessor<T, Self>,
        when: wasi::clocks::monotonic_clock::Mark,
    ) -> Result<()> {
        let (timer, cpu, runtime_state) = accessor.with(|mut access| {
            let store = access.get();
            (
                store.timer(),
                store.cpu.clone(),
                store.runtime_state.clone(),
            )
        });
        crate::wait_until_runtime_deadline(timer, cpu, runtime_state, when).await;
        Ok(())
    }

    async fn wait_for<T: Send>(
        accessor: &Accessor<T, Self>,
        duration: wasi::clocks::types::Duration,
    ) -> Result<()> {
        let deadline =
            accessor.with(|mut access| access.get().now_nanos().saturating_add(duration));
        Self::wait_until(accessor, deadline).await
    }
}

impl<CpuImpl, HostFs> wasi::clocks::system_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<wasi::clocks::system_clock::Instant> {
        Ok(system_time_from_nanos(self.now_nanos()))
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl<CpuImpl, HostFs> wasi::cli::environment::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments().to_vec())
    }

    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment().to_vec())
    }

    fn get_initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::exit::Host for StoreData<CpuImpl, HostFs>
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
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi exit code {code}"
        )))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        self.request_exit(status_code);
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi exit code {status_code}"
        )))
    }
}

impl<CpuImpl, HostFs> wasi::cli::stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> wasi::cli::stdin::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream<T>(
        mut access: Access<'_, T, Self>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), cli_types::ErrorCode>>,
    )> {
        use crate::ComponentOutputMode;
        // For spawn-mode programs, drain the parent-provided stdin reader
        // one chunk at a time. For Serial/Trace programs, hand back an
        // immediately empty stream (serial stdin is polled through the
        // dedicated `helios:system/serial` interface instead).
        let stream = match access.get().output_mode() {
            ComponentOutputMode::Child { stdin_rx, .. } => {
                let reader = stdin_rx.clone();
                StreamReader::new(&mut access, ChannelStreamProducer::new(reader))?
            }
            ComponentOutputMode::Serial | ComponentOutputMode::Trace => {
                StreamReader::new(&mut access, Vec::<u8>::new())?
            }
        };
        let future = FutureReader::new(&mut access, async {
            Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(()))
        })?;
        Ok((stream, future))
    }
}

/// Bridges a kernel [`ByteReader`](crate::ByteReader) to a wasmtime
/// component stream producer. Used for both `wasi:cli/stdin.read-via-stream`
/// (when spawn-mode hooks the child's stdin to the parent channel) and
/// `child.stdout` / `child.stderr` on the parent side.
///
/// Because `ByteReader::read` is async and `poll_produce` is sync, we
/// keep a pinned boxed future representing the in-flight read; every
/// poll drives it until a chunk is produced.
pub(crate) struct ChannelStreamProducer {
    reader: crate::ByteReader,
    pending: Option<Pin<Box<dyn core::future::Future<Output = Option<Vec<u8>>> + Send>>>,
    completion: Option<oneshot::Sender<()>>,
}

impl ChannelStreamProducer {
    pub(crate) fn new(reader: crate::ByteReader) -> Self {
        Self {
            reader,
            pending: None,
            completion: None,
        }
    }

    pub(crate) fn new_with_completion(
        reader: crate::ByteReader,
        completion: oneshot::Sender<()>,
    ) -> Self {
        Self {
            reader,
            pending: None,
            completion: Some(completion),
        }
    }

    fn finish(&mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(());
        }
    }
}

impl Drop for ChannelStreamProducer {
    fn drop(&mut self) {
        self.finish();
    }
}

impl<T> StreamProducer<T> for ChannelStreamProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: wasmtime::StoreContextMut<'_, T>,
        mut destination: Destination<'_, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if finish {
            self.finish();
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        loop {
            if self.pending.is_none() {
                let reader = self.reader.clone();
                self.pending = Some(Box::pin(async move { reader.read().await }));
            }
            let fut = self
                .pending
                .as_mut()
                .expect("pending future was just installed");
            match fut.as_mut().poll(cx) {
                Poll::Pending => {
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    self.pending = None;
                    self.finish();
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Ready(Some(bytes)) => {
                    self.pending = None;
                    let bytes: Vec<u8> = bytes;
                    if bytes.is_empty() {
                        continue;
                    }
                    destination.set_buffer(VecBuffer::from(bytes));
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
            }
        }
    }
}

/// Bridges a wasmtime component stream consumer to a kernel
/// [`ByteWriter`](crate::ByteWriter). Used for `child.stdin` so the
/// parent-supplied stream is copied into the child's stdin channel.
pub(crate) struct ChannelStreamConsumer {
    writer: crate::ByteWriter,
    completion: Option<oneshot::Sender<core::result::Result<(), ()>>>,
}

impl ChannelStreamConsumer {
    pub(crate) fn new(
        writer: crate::ByteWriter,
        completion: oneshot::Sender<core::result::Result<(), ()>>,
    ) -> Self {
        Self {
            writer,
            completion: Some(completion),
        }
    }

    fn finish(&mut self, result: core::result::Result<(), ()>) {
        if let Some(tx) = self.completion.take() {
            let _ = tx.send(result);
        }
    }
}

impl Drop for ChannelStreamConsumer {
    fn drop(&mut self) {
        self.finish(Ok(()));
    }
}

impl<T: 'static> StreamConsumer<T> for ChannelStreamConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;
        match self.as_ref().get_ref().writer.write(bytes) {
            Ok(()) => Poll::Ready(Ok(StreamResult::Completed)),
            Err(_closed) => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}

impl<CpuImpl, HostFs> wasi::cli::stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> wasi::cli::stdout::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn write_via_stream<T>(
        mut access: Access<'_, T, Self>,
        data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), cli_types::ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let getter = access.getter();
        data.pipe(
            &mut access,
            SerialStreamConsumer::new(getter, tx, OutputStreamKind::Stdout),
        )?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(())),
            }
        })
    }
}

impl<CpuImpl, HostFs> wasi::cli::stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> wasi::cli::stderr::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn write_via_stream<T>(
        mut access: Access<'_, T, Self>,
        data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), cli_types::ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let getter = access.getter();
        data.pipe(
            &mut access,
            SerialStreamConsumer::new(getter, tx, OutputStreamKind::Stderr),
        )?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(())),
            }
        })
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_input::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> wasi::cli::terminal_input::HostTerminalInput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_output::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> wasi::cli::terminal_output::HostTerminalOutput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::random::random::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0_u8; len as usize])
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl<CpuImpl, HostFs> wasi::random::insecure::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0_u8; len as usize])
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl<CpuImpl, HostFs> wasi::random::insecure_seed::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }
}

impl<CpuImpl, HostFs> wasi::filesystem::preopens::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_directories(&mut self) -> Result<Vec<(Resource<FsDescriptor>, String)>> {
        let descriptor = self.filesystem().root_descriptor();
        let resource = self.table.push(descriptor)?;
        Ok(vec![(resource, String::from("/"))])
    }
}

impl<CpuImpl, HostFs> wasi::filesystem::types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn convert_error_code(&mut self, error: FsError) -> Result<fs_types::ErrorCode> {
        error.downcast()
    }
}
impl<CpuImpl, HostFs> wasi::filesystem::types::HostDescriptor for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, descriptor: Resource<FsDescriptor>) -> Result<()> {
        self.table.delete(descriptor)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::filesystem::types::HostDescriptorWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), fs_types::ErrorCode>>,
    )> {
        let getter = accessor.getter();
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor)?;
        let (tx, rx) = oneshot::channel();
        let stream = StreamReader::new(
            &mut accessor,
            FileReadStreamProducer::new(getter, descriptor, offset, FILE_READ_CHUNK_BYTES, tx),
        )?;
        let future = FutureReader::new(&mut accessor, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(())),
            }
        })?;
        Ok((stream, future))
    }

    fn write_via_stream<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
        mut data: StreamReader<u8>,
        offset: u64,
    ) -> Result<FutureReader<core::result::Result<(), fs_types::ErrorCode>>> {
        let offset: usize = offset
            .try_into()
            .map_err(|_| wasmtime::Error::msg("file offset does not fit into usize"))?;
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor);
        let getter = accessor.getter();
        match descriptor {
            Ok(descriptor) => {
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut accessor,
                    FileWriteConsumer::new_at(getter, descriptor, offset, tx),
                )?;
                FutureReader::new(&mut accessor, async move {
                    match rx.await {
                        Ok(result) => Ok::<_, wasmtime::Error>(result),
                        Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(())),
                    }
                })
            }
            Err(error) => {
                data.close(&mut accessor)?;
                FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })
            }
        }
    }

    fn append_via_stream<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
        mut data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), fs_types::ErrorCode>>> {
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor);
        let getter = accessor.getter();
        match descriptor {
            Ok(descriptor) => {
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut accessor,
                    FileWriteConsumer::new_append(getter, descriptor, tx),
                )?;
                FutureReader::new(&mut accessor, async move {
                    match rx.await {
                        Ok(result) => Ok::<_, wasmtime::Error>(result),
                        Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(())),
                    }
                })
            }
            Err(error) => {
                data.close(&mut accessor)?;
                FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })
            }
        }
    }

    async fn advise<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: fs_types::Advice,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn sync_data<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
    ) -> Result<(), FsError> {
        Ok(())
    }

    async fn get_flags<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorFlags, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            Ok(descriptor.flags)
        })
    }

    async fn get_type<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorType, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let kind = match descriptor.kind {
                FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                FsNodeKind::File => fs_types::DescriptorType::RegularFile,
            };
            Ok(kind)
        })
    }

    async fn set_size<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: u64,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn set_times<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: fs_types::NewTimestamp,
        _: fs_types::NewTimestamp,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    fn read_directory<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<(
        StreamReader<fs_types::DirectoryEntry>,
        FutureReader<core::result::Result<(), fs_types::ErrorCode>>,
    )> {
        let entries = {
            let descriptor = get_fs_descriptor(accessor.get(), &descriptor)?;
            accessor.get().filesystem().read_directory(&descriptor.path)
        };
        match entries {
            Ok(entries) => {
                let stream = StreamReader::new(&mut accessor, entries)?;
                let future = FutureReader::new(&mut accessor, async {
                    Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(()))
                })?;
                Ok((stream, future))
            }
            Err(error) => {
                let stream =
                    StreamReader::new(&mut accessor, Vec::<fs_types::DirectoryEntry>::new())?;
                let future = FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })?;
                Ok((stream, future))
            }
        }
    }

    async fn sync<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
    ) -> Result<(), FsError> {
        Ok(())
    }

    async fn create_directory_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        let (absolute, base_flags, base_kind) = accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            let absolute = crate::resolve_child_path(&base.path, &path)
                .map_err(map_component_fs_path_error)?;
            Ok::<_, FsError>((absolute, base.flags, base.kind))
        })?;
        if !base_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
            return Err(fs_types::ErrorCode::ReadOnly.into());
        }
        if base_kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory.into());
        }
        if let Some(host_path) = crate::guest_host_share_path(&absolute).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .create_directory(&host_path)
                .await
                .map_err(map_host_fs_error)?;
            accessor.with(|mut access| {
                access
                    .get()
                    .filesystem_mut()
                    .invalidate_host_subtree(&absolute);
            });
            return Ok(());
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let now_nanos = access.get().now_nanos();
            access
                .get()
                .filesystem_mut()
                .create_directory_at(&descriptor, &path, now_nanos)
                .map_err(Into::into)
        })
    }

    async fn stat<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        // Extract path from the descriptor synchronously, then do the
        // (potentially async) FS operation outside the accessor so the
        // kernel executor can drive the 9p transport concurrently.
        let path = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            Ok::<_, FsError>(descriptor.path.clone())
        })?;
        let host_path = crate::guest_host_share_path(&path).map(|p| p.to_owned());
        if let Some(host_path) = host_path {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            let metadata = service
                .stat_path(&host_path)
                .await
                .map_err(map_host_fs_error)?;
            return Ok(fs_types::DescriptorStat {
                type_: if metadata.qid_type & 0x80 != 0 {
                    fs_types::DescriptorType::Directory
                } else {
                    fs_types::DescriptorType::RegularFile
                },
                link_count: 1,
                size: metadata.size,
                data_access_timestamp: None,
                data_modification_timestamp: None,
                status_change_timestamp: None,
            });
        }
        accessor.with(|mut access| access.get().filesystem().stat(&path).map_err(Into::into))
    }

    async fn stat_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        // Guest filesystem has no symlinks; SYMLINK_FOLLOW is a no-op.
        let _ = path_flags;
        let absolute = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            crate::resolve_child_path(&descriptor.path, &path)
                .map_err(map_component_fs_path_error)
                .map_err(FsError::from)
        })?;
        let host_path = crate::guest_host_share_path(&absolute).map(|p| p.to_owned());
        if let Some(host_path) = host_path {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            let metadata = service
                .stat_path(&host_path)
                .await
                .map_err(map_host_fs_error)?;
            return Ok(fs_types::DescriptorStat {
                type_: if metadata.qid_type & 0x80 != 0 {
                    fs_types::DescriptorType::Directory
                } else {
                    fs_types::DescriptorType::RegularFile
                },
                link_count: 1,
                size: metadata.size,
                data_access_timestamp: None,
                data_modification_timestamp: None,
                status_change_timestamp: None,
            });
        }
        accessor.with(|mut access| {
            access
                .get()
                .filesystem()
                .stat(&absolute)
                .map_err(Into::into)
        })
    }

    async fn set_times_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: fs_types::PathFlags,
        _: String,
        _: fs_types::NewTimestamp,
        _: fs_types::NewTimestamp,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn link_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: fs_types::PathFlags,
        _: String,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn open_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
        open_flags: fs_types::OpenFlags,
        flags: fs_types::DescriptorFlags,
    ) -> Result<Resource<FsDescriptor>, FsError> {
        // Extract base descriptor data and resolve absolute path synchronously.
        let (base_path, base_kind, base_flags) = accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            Ok::<_, FsError>((base.path.clone(), base.kind, base.flags))
        })?;

        // No symlinks in our guest FS — SYMLINK_FOLLOW is a no-op.
        let _ = path_flags;
        if base_kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory.into());
        }

        let absolute =
            crate::resolve_child_path(&base_path, &path).map_err(map_component_fs_path_error)?;
        let host_path = crate::guest_host_share_path(&absolute).map(|p| p.to_owned());

        if let Some(host_path) = host_path {
            // Host filesystem path — do async I/O outside accessor.
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;

            let metadata = service.stat_path(&host_path).await;
            match metadata {
                Ok(metadata) => {
                    let kind = if metadata.qid_type & 0x80 != 0 {
                        FsNodeKind::Directory
                    } else {
                        FsNodeKind::File
                    };
                    if open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
                        && open_flags.contains(fs_types::OpenFlags::CREATE)
                    {
                        return Err(fs_types::ErrorCode::Exist.into());
                    }
                    if open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                        && kind != FsNodeKind::Directory
                    {
                        return Err(fs_types::ErrorCode::NotDirectory.into());
                    }
                    if !open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                        && kind == FsNodeKind::Directory
                    {
                        return Err(fs_types::ErrorCode::IsDirectory.into());
                    }
                    if open_flags.contains(fs_types::OpenFlags::TRUNCATE) {
                        if kind != FsNodeKind::File {
                            return Err(fs_types::ErrorCode::IsDirectory.into());
                        }
                        if !base_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
                            return Err(fs_types::ErrorCode::ReadOnly.into());
                        }
                        service
                            .truncate_file(&host_path)
                            .await
                            .map_err(map_host_fs_error)?;
                    }
                    // For host files, eagerly read the content so that
                    // subsequent stream reads work synchronously without
                    // needing async 9p I/O inside poll_produce.
                    if kind == FsNodeKind::File {
                        let content = service
                            .read_file(&host_path)
                            .await
                            .map_err(map_host_fs_error)?;
                        let opened = FsDescriptor {
                            path: absolute.clone(),
                            kind,
                            flags,
                        };
                        return accessor.with(|mut access| {
                            access
                                .get()
                                .filesystem_mut()
                                .seed_host_file_content(&absolute, content);
                            access.get().table.push(opened).map_err(FsError::trap)
                        });
                    }
                    // Directory — eagerly seed its direct children into the
                    // embedded FS so read_directory can walk them without
                    // re-entering async 9p I/O.
                    let entries = service
                        .read_dir(&host_path)
                        .await
                        .map_err(map_host_fs_error)?;
                    let opened = FsDescriptor {
                        path: absolute.clone(),
                        kind,
                        flags,
                    };
                    return accessor.with(|mut access| {
                        access
                            .get()
                            .filesystem_mut()
                            .seed_host_directory_entries(&absolute, entries);
                        access.get().table.push(opened).map_err(FsError::trap)
                    });
                }
                Err(err) => {
                    let err = map_host_fs_error(err);
                    if matches!(err, fs_types::ErrorCode::NoEntry)
                        && open_flags.contains(fs_types::OpenFlags::CREATE)
                    {
                        if !base_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
                            return Err(fs_types::ErrorCode::ReadOnly.into());
                        }
                        service
                            .create_file(&host_path)
                            .await
                            .map_err(map_host_fs_error)?;
                        let opened = FsDescriptor {
                            path: absolute,
                            kind: FsNodeKind::File,
                            flags,
                        };
                        return accessor.with(|mut access| {
                            access.get().table.push(opened).map_err(FsError::trap)
                        });
                    }
                    return Err(err.into());
                }
            }
        }

        // Embedded filesystem path — fully synchronous.
        accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            let now_nanos = access.get().now_nanos();
            let opened = access
                .get()
                .filesystem_mut()
                .open_at(&base, path_flags, &path, open_flags, flags, now_nanos)
                .map_err(FsError::from)?;
            let resource = access.get().table.push(opened).map_err(FsError::trap)?;
            Ok(resource)
        })
    }

    async fn readlink_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<String, FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn remove_directory_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        let (absolute, base_flags) = accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            let absolute = crate::resolve_child_path(&base.path, &path)
                .map_err(map_component_fs_path_error)?;
            Ok::<_, FsError>((absolute, base.flags))
        })?;
        if !base_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
            return Err(fs_types::ErrorCode::ReadOnly.into());
        }
        if let Some(host_path) = crate::guest_host_share_path(&absolute).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .remove(&host_path, true)
                .await
                .map_err(map_host_fs_error)?;
            // Drop any cached embedded entries that mirrored this host path.
            accessor.with(|mut access| {
                access
                    .get()
                    .filesystem_mut()
                    .invalidate_host_subtree(&absolute);
            });
            return Ok(());
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            access
                .get()
                .filesystem_mut()
                .remove_directory_at(&descriptor, &path)
                .map_err(Into::into)
        })
    }

    async fn rename_at<T: Send>(
        accessor: &Accessor<T, Self>,
        source_descriptor: Resource<FsDescriptor>,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<(), FsError> {
        let (source_absolute, destination_absolute, source_flags, destination_flags) = accessor
            .with(|mut access| {
                let source_base = get_fs_descriptor(access.get(), &source_descriptor)?;
                let destination_base = get_fs_descriptor(access.get(), &destination_descriptor)?;
                let source_absolute = crate::resolve_child_path(&source_base.path, &source_path)
                    .map_err(map_component_fs_path_error)?;
                let destination_absolute =
                    crate::resolve_child_path(&destination_base.path, &destination_path)
                        .map_err(map_component_fs_path_error)?;
                Ok::<_, FsError>((
                    source_absolute,
                    destination_absolute,
                    source_base.flags,
                    destination_base.flags,
                ))
            })?;
        if !source_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            || !destination_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly.into());
        }
        let source_host = crate::guest_host_share_path(&source_absolute).map(|p| p.to_owned());
        let destination_host =
            crate::guest_host_share_path(&destination_absolute).map(|p| p.to_owned());
        if source_host.is_some() || destination_host.is_some() {
            let Some(source_host) = source_host else {
                return Err(fs_types::ErrorCode::CrossDevice.into());
            };
            let Some(destination_host) = destination_host else {
                return Err(fs_types::ErrorCode::CrossDevice.into());
            };
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .rename(&source_host, &destination_host)
                .await
                .map_err(map_host_fs_error)?;
            accessor.with(|mut access| {
                let fs = access.get().filesystem_mut();
                fs.invalidate_host_subtree(&source_absolute);
                fs.invalidate_host_subtree(&destination_absolute);
            });
            return Ok(());
        }
        accessor.with(|mut access| {
            let source_base = get_fs_descriptor(access.get(), &source_descriptor)?;
            let destination_base = get_fs_descriptor(access.get(), &destination_descriptor)?;
            let now_nanos = access.get().now_nanos();
            access
                .get()
                .filesystem_mut()
                .rename_at(
                    &source_base,
                    &source_path,
                    &destination_base,
                    &destination_path,
                    now_nanos,
                )
                .map_err(Into::into)
        })
    }

    async fn symlink_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: String,
        _: String,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn unlink_file_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        let (absolute, base_flags) = accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            let absolute = crate::resolve_child_path(&base.path, &path)
                .map_err(map_component_fs_path_error)?;
            Ok::<_, FsError>((absolute, base.flags))
        })?;
        if !base_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
            return Err(fs_types::ErrorCode::ReadOnly.into());
        }
        if let Some(host_path) = crate::guest_host_share_path(&absolute).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .remove(&host_path, false)
                .await
                .map_err(map_host_fs_error)?;
            accessor.with(|mut access| {
                access
                    .get()
                    .filesystem_mut()
                    .invalidate_host_subtree(&absolute);
            });
            return Ok(());
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            access
                .get()
                .filesystem_mut()
                .unlink_file_at(&descriptor, &path)
                .map_err(Into::into)
        })
    }

    async fn is_same_object<T: Send>(
        accessor: &Accessor<T, Self>,
        a: Resource<FsDescriptor>,
        b: Resource<FsDescriptor>,
    ) -> Result<bool> {
        accessor.with(|mut access| {
            let left = access.get().table.get(&a)?.clone();
            let right = access.get().table.get(&b)?.clone();
            Ok(left.path == right.path && left.kind == right.kind)
        })
    }

    async fn metadata_hash<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::MetadataHashValue, FsError> {
        let path = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            Ok::<_, FsError>(descriptor.path.clone())
        })?;
        if let Some(host_path) = crate::guest_host_share_path(&path).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            let metadata = service
                .stat_path(&host_path)
                .await
                .map_err(map_host_fs_error)?;
            return Ok(fs_types::MetadataHashValue {
                lower: metadata.qid_path,
                upper: u64::from(metadata.mode) << 32 ^ metadata.size,
            });
        }
        accessor.with(|mut access| {
            access
                .get()
                .filesystem_mut()
                .metadata_hash(&path)
                .map_err(Into::into)
        })
    }

    async fn metadata_hash_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::MetadataHashValue, FsError> {
        // No symlinks in our guest FS — SYMLINK_FOLLOW is a no-op.
        let _ = path_flags;
        let absolute = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            crate::resolve_child_path(&descriptor.path, &path)
                .map_err(map_component_fs_path_error)
                .map_err(FsError::from)
        })?;
        if let Some(host_path) = crate::guest_host_share_path(&absolute).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            let metadata = service
                .stat_path(&host_path)
                .await
                .map_err(map_host_fs_error)?;
            return Ok(fs_types::MetadataHashValue {
                lower: metadata.qid_path,
                upper: u64::from(metadata.mode) << 32 ^ metadata.size,
            });
        }
        accessor.with(|mut access| {
            access
                .get()
                .filesystem_mut()
                .metadata_hash(&absolute)
                .map_err(Into::into)
        })
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> wasi::sockets::types::HostTcpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn bind(
        &mut self,
        _: Resource<TcpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn create(
        &mut self,
        _: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<TcpSocket>, socket_types::ErrorCode>> {
        Ok(Err(socket_types::ErrorCode::NotSupported))
    }

    fn get_local_address(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_remote_address(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_is_listening(&mut self, _: Resource<TcpSocket>) -> Result<bool> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_address_family(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_listen_backlog_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_enabled(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<bool, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_enabled(
        &mut self,
        _: Resource<TcpSocket>,
        _: bool,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<wasi::clocks::types::Duration, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSocket>,
        _: wasi::clocks::types::Duration,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_interval(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<wasi::clocks::types::Duration, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_interval(
        &mut self,
        _: Resource<TcpSocket>,
        _: wasi::clocks::types::Duration,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_count(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u32, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_count(
        &mut self,
        _: Resource<TcpSocket>,
        _: u32,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_hop_limit(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_hop_limit(
        &mut self,
        _: Resource<TcpSocket>,
        _: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_receive_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_receive_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_send_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_send_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn drop(&mut self, resource: Resource<TcpSocket>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::HostUdpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn bind(
        &mut self,
        socket: Resource<UdpSocket>,
        local_address: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        let local_address = match parse_p3_udp_socket_address(local_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
        };
        let result = socket
            .bind(local_address)
            .await
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }

    async fn connect(
        &mut self,
        socket: Resource<UdpSocket>,
        remote_address: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        let remote_address = match parse_p3_udp_socket_address(remote_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
        };
        let result = socket
            .connect(remote_address)
            .await
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }

    fn create(
        &mut self,
        address_family: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<UdpSocket>, socket_types::ErrorCode>> {
        let family = match map_p3_udp_family(address_family) {
            Ok(family) => family,
            Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
        };
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(socket_types::ErrorCode::Other(Some(String::from(
                "network service is unavailable on this machine",
            )))));
        };
        let resource = self.table.push(UdpSocket::new(service, family))?;
        Ok(Ok(resource))
    }

    fn disconnect(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.disconnect().map_err(map_p3_udp_socket_error))
    }

    fn get_local_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .local_address()
            .map(format_p3_udp_socket_address)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .remote_address()
            .map(format_p3_udp_socket_address)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_address_family(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        let socket = self.table.get(&socket)?.clone();
        Ok(format_p3_udp_family(socket.family()))
    }

    fn get_unicast_hop_limit(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.unicast_hop_limit().map_err(map_p3_udp_socket_error))
    }

    fn set_unicast_hop_limit(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_unicast_hop_limit(value)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_receive_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .receive_buffer_size()
            .map_err(map_p3_udp_socket_error))
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_receive_buffer_size(value)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_send_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.send_buffer_size().map_err(map_p3_udp_socket_error))
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_send_buffer_size(value)
            .map_err(map_p3_udp_socket_error))
    }

    fn drop(&mut self, resource: Resource<UdpSocket>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::HostTcpSocketWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn connect<T>(
        _: &Accessor<T, Self>,
        _: Resource<TcpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn listen<T>(
        _: Access<'_, T, Self>,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<StreamReader<Resource<TcpSocket>>, socket_types::ErrorCode>>
    {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn send<T>(
        _: Access<'_, T, Self>,
        _: Resource<TcpSocket>,
        _: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), socket_types::ErrorCode>>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn receive<T>(
        _: Access<'_, T, Self>,
        _: Resource<TcpSocket>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), socket_types::ErrorCode>>,
    )> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::HostUdpSocketWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn send<T>(
        accessor: &Accessor<T, Self>,
        socket: Resource<UdpSocket>,
        bytes: Vec<u8>,
        remote_address: Option<socket_types::IpSocketAddress>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = accessor
            .with(|mut access| access.get().table.get(&socket).map(|socket| socket.clone()))?;
        let remote_address = match remote_address {
            Some(address) => match parse_p3_udp_socket_address(address, socket.family()) {
                Ok(address) => Some(address),
                Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
            },
            None => None,
        };
        let result = socket
            .send_datagram(&bytes, remote_address, u64::MAX)
            .await
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }

    async fn receive<T>(
        accessor: &Accessor<T, Self>,
        socket: Resource<UdpSocket>,
    ) -> Result<
        core::result::Result<(Vec<u8>, socket_types::IpSocketAddress), socket_types::ErrorCode>,
    > {
        let socket = accessor
            .with(|mut access| access.get().table.get(&socket).map(|socket| socket.clone()))?;
        let result = socket
            .receive_datagram(None, u64::MAX)
            .await
            .map(|datagram| {
                (
                    datagram.bytes,
                    format_p3_udp_socket_address(datagram.remote_address),
                )
            })
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }
}

fn map_p3_udp_family(
    family: socket_types::IpAddressFamily,
) -> core::result::Result<WasiUdpSocketFamily, WasiUdpSocketError> {
    match family {
        socket_types::IpAddressFamily::Ipv4 => Ok(WasiUdpSocketFamily::Ipv4),
        socket_types::IpAddressFamily::Ipv6 => Err(WasiUdpSocketError::NotSupported),
    }
}

fn format_p3_udp_family(family: WasiUdpSocketFamily) -> socket_types::IpAddressFamily {
    match family {
        WasiUdpSocketFamily::Ipv4 => socket_types::IpAddressFamily::Ipv4,
    }
}

fn parse_p3_udp_socket_address(
    address: socket_types::IpSocketAddress,
    family: WasiUdpSocketFamily,
) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
    match (family, address) {
        (WasiUdpSocketFamily::Ipv4, socket_types::IpSocketAddress::Ipv4(address)) => {
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
        (WasiUdpSocketFamily::Ipv4, socket_types::IpSocketAddress::Ipv6(_)) => {
            Err(WasiUdpSocketError::NotSupported)
        }
    }
}

fn format_p3_udp_socket_address(address: WasiUdpSocketAddress) -> socket_types::IpSocketAddress {
    let [a, b, c, d] = address.address.octets();
    socket_types::IpSocketAddress::Ipv4(socket_types::Ipv4SocketAddress {
        port: address.port,
        address: (a, b, c, d),
    })
}

fn map_p3_udp_socket_error(error: WasiUdpSocketError) -> socket_types::ErrorCode {
    match error {
        WasiUdpSocketError::NotSupported => socket_types::ErrorCode::NotSupported,
        WasiUdpSocketError::InvalidArgument => socket_types::ErrorCode::InvalidArgument,
        WasiUdpSocketError::InvalidState | WasiUdpSocketError::NotInProgress => {
            socket_types::ErrorCode::InvalidState
        }
        WasiUdpSocketError::AddressNotBindable => socket_types::ErrorCode::AddressNotBindable,
        WasiUdpSocketError::DatagramTooLarge => socket_types::ErrorCode::DatagramTooLarge,
        WasiUdpSocketError::WouldBlock
        | WasiUdpSocketError::Backend(crate::UdpError {
            kind: crate::UdpErrorKind::Timeout,
            ..
        }) => socket_types::ErrorCode::Timeout,
        WasiUdpSocketError::Backend(error) => socket_types::ErrorCode::Other(Some(error.detail)),
    }
}

impl<CpuImpl, HostFs> wasi::sockets::ip_name_lookup::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> wasi::sockets::ip_name_lookup::HostWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn resolve_addresses<T: Send>(
        _: &Accessor<T, Self>,
        _: String,
    ) -> Result<core::result::Result<Vec<socket_types::IpAddress>, ip_name_lookup::ErrorCode>> {
        Ok(Err(ip_name_lookup::ErrorCode::Other(Some(String::from(
            "ip-name-lookup is unsupported on the embedded debugger host",
        )))))
    }
}

fn system_time_from_nanos(nanos: u64) -> wasi::clocks::system_clock::Instant {
    let seconds = nanos / 1_000_000_000;
    let nanoseconds = (nanos % 1_000_000_000) as u32;
    wasi::clocks::system_clock::Instant {
        seconds: seconds
            .try_into()
            .expect("debugger wall clock exceeded wasi system clock range"),
        nanoseconds,
    }
}

fn get_fs_descriptor<CpuImpl, HostFs>(
    store: &mut StoreData<CpuImpl, HostFs>,
    resource: &Resource<FsDescriptor>,
) -> core::result::Result<FsDescriptor, fs_types::ErrorCode>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store
        .table
        .get(resource)
        .cloned()
        .map_err(fs_resource_error)
}

pub(crate) fn fs_resource_error(
    error: wasmtime::component::ResourceTableError,
) -> fs_types::ErrorCode {
    let mapped = match error {
        wasmtime::component::ResourceTableError::NotPresent => {
            crate::ComponentResourceTableError::NotPresent
        }
        wasmtime::component::ResourceTableError::WrongType => {
            crate::ComponentResourceTableError::WrongType
        }
        wasmtime::component::ResourceTableError::HasChildren => {
            crate::ComponentResourceTableError::HasChildren
        }
        wasmtime::component::ResourceTableError::Full => crate::ComponentResourceTableError::Full,
    };
    match crate::map_resource_table_error(mapped) {
        crate::ComponentFsResourceError::BadDescriptor => fs_types::ErrorCode::BadDescriptor,
        crate::ComponentFsResourceError::Busy => fs_types::ErrorCode::Busy,
        crate::ComponentFsResourceError::Overflow => fs_types::ErrorCode::Overflow,
    }
}

fn is_dir_first(kind: fs_types::DescriptorType) -> u8 {
    match kind {
        fs_types::DescriptorType::Directory => 0,
        _ => 1,
    }
}

fn map_component_fs_path_error(error: crate::ComponentFsPathError) -> fs_types::ErrorCode {
    match error {
        crate::ComponentFsPathError::InvalidBasePath => fs_types::ErrorCode::Invalid,
        crate::ComponentFsPathError::NotPermitted => fs_types::ErrorCode::NotPermitted,
    }
}

pub(crate) fn map_host_fs_error(error: crate::HostFsError) -> fs_types::ErrorCode {
    match error.kind() {
        HostFsErrorKind::NoEntry => fs_types::ErrorCode::NoEntry,
        HostFsErrorKind::Exist => fs_types::ErrorCode::Exist,
        HostFsErrorKind::NotDirectory => fs_types::ErrorCode::NotDirectory,
        HostFsErrorKind::IsDirectory => fs_types::ErrorCode::IsDirectory,
        HostFsErrorKind::NotEmpty => fs_types::ErrorCode::NotEmpty,
        HostFsErrorKind::ReadOnly => fs_types::ErrorCode::ReadOnly,
        HostFsErrorKind::Unsupported => fs_types::ErrorCode::Unsupported,
        HostFsErrorKind::Io => fs_types::ErrorCode::Io,
    }
}

#[cfg(test)]
mod tests {
    use crate::{EmbeddedBootFile, EmbeddedBootFs};

    use super::{DebugFileSystem, FsNodeKind, bindings};

    #[test]
    fn create_directory_adds_node() {
        let mut filesystem = DebugFileSystem::new();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 7)
            .expect("directory creation must succeed");

        let node = filesystem
            .get_node("/tmp")
            .expect("directory node must exist after creation");
        assert_eq!(node.kind, FsNodeKind::Directory);
        assert_eq!(node.modified_nanos, 7);
    }

    #[test]
    fn create_directory_rejects_existing_path() {
        let mut filesystem = DebugFileSystem::new();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 1)
            .expect("initial directory creation must succeed");

        let error = filesystem
            .create_directory_at(&root, "tmp", 2)
            .expect_err("creating the same directory twice must fail");
        assert_eq!(error, bindings::wasi::filesystem::types::ErrorCode::Exist);
    }

    #[test]
    fn bootfs_seed_adds_readonly_programs() {
        let mut filesystem = DebugFileSystem::new();
        let bootfs = EmbeddedBootFs::new(&[EmbeddedBootFile::new("bin/tool", b"tool")]);
        filesystem.seed_bootfs(bootfs);

        let program = filesystem
            .get_node("/bin/tool")
            .expect("bootfs program must be present");
        assert_eq!(program.kind, FsNodeKind::File);
        assert!(program.readonly);

        let directory = filesystem
            .get_node("/bin")
            .expect("bootfs program directory must be present");
        assert_eq!(directory.kind, FsNodeKind::Directory);
        assert!(directory.readonly);
    }

    #[test]
    fn readonly_bootfs_file_rejects_writes() {
        let mut filesystem = DebugFileSystem::new();
        let bootfs = EmbeddedBootFs::new(&[EmbeddedBootFile::new("bin/tool", b"tool")]);
        filesystem.seed_bootfs(bootfs);
        let root = filesystem.root_descriptor();

        let descriptor = filesystem
            .open_at(
                &root,
                bindings::wasi::filesystem::types::PathFlags::empty(),
                "bin/tool",
                bindings::wasi::filesystem::types::OpenFlags::empty(),
                bindings::wasi::filesystem::types::DescriptorFlags::WRITE,
                0,
            )
            .expect("opening readonly bootfs file must succeed for lookup");

        let error = filesystem
            .write_at(&descriptor, 0, b"x", 1)
            .expect_err("readonly bootfs file must reject writes");
        assert_eq!(
            error,
            bindings::wasi::filesystem::types::ErrorCode::ReadOnly
        );
    }

    #[test]
    fn opening_existing_directory_without_directory_flag_fails() {
        let mut filesystem = DebugFileSystem::new();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 1)
            .expect("directory creation must succeed");

        let error = filesystem
            .open_at(
                &root,
                bindings::wasi::filesystem::types::PathFlags::empty(),
                "tmp",
                bindings::wasi::filesystem::types::OpenFlags::CREATE,
                bindings::wasi::filesystem::types::DescriptorFlags::WRITE,
                2,
            )
            .expect_err("opening an existing directory as a file must fail");
        assert_eq!(
            error,
            bindings::wasi::filesystem::types::ErrorCode::IsDirectory
        );
    }
}
