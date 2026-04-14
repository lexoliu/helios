extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, HasSelf, Linker, Resource, Source, StreamConsumer,
    StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{self, Result};
use crate::{EmbeddedBootFile, EmbeddedBootFs, HostFsErrorKind, embedded_init, yield_now};

use crate::component_host::{OutputStreamKind, StoreData};

pub(crate) type FsNodeKind = crate::ComponentFsNodeKind;
const FILE_READ_CHUNK_BYTES: usize = 1024 * 1024;

pub(crate) mod p2;

pub(crate) mod bindings;

use bindings as wasi;
use wasi::cli::types as cli_types;
use wasi::filesystem::types as fs_types;
use wasi::sockets::ip_name_lookup;
use wasi::sockets::types as socket_types;

pub(crate) fn add_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{

    wasi::clocks::monotonic_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::clocks::system_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::environment::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::exit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, &Default::default(), |state| state)?;
    wasi::cli::stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::terminal_input::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::terminal_output::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::terminal_stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::terminal_stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::cli::terminal_stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::random::random::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::random::insecure::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::random::insecure_seed::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::filesystem::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::filesystem::preopens::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::sockets::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    wasi::sockets::ip_name_lookup::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    Ok(())
}

#[derive(Clone)]
struct FsNode {
    path: String,
    kind: FsNodeKind,
    contents: Vec<u8>,
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
pub struct UdpSocket;

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

pub(crate) struct DebugFileSystem<State, HostFsService> {
    nodes: Vec<FsNode>,
    next_inode: u64,
    runtime_state: State,
    _host_fs: PhantomData<HostFsService>,
}

struct SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    stream: OutputStreamKind,
    result: Option<oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>>,
}

impl<T, CpuImpl, HostFs> SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamConsumer<T> for SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    descriptor: FsDescriptor,
    mode: FileWriteMode,
    result: Option<oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>>,
}

struct FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamProducer<T>
    for FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
        match store_data
            .filesystem()
            .read_file_chunk(&self.descriptor, self.offset, self.chunk_bytes)
        {
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamConsumer<T> for FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
                let result = store_data
                    .filesystem_mut()
                    .write_at(&descriptor, *offset, &bytes, now_nanos);
                if result.is_ok() {
                    *offset = offset
                        .checked_add(bytes.len())
                        .expect("file write offset overflowed usize");
                }
                result
            }
            FileWriteMode::Append => store_data.filesystem_mut().append(&descriptor, &bytes, now_nanos),
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
                contents: Vec::new(),
                inode: 1,
                modified_nanos: 0,
                readonly: false,
            }],
            next_inode: 2,
            runtime_state,
            _host_fs: PhantomData,
        };

        if let Some(init) = embedded_init() {
            filesystem.seed_bootfs(init.bootfs());
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
            contents: file.contents().to_vec(),
            inode,
            modified_nanos: 0,
            readonly: true,
        });
    }

    /// Cache host file content in the embedded FS so that subsequent stream
    /// reads can proceed synchronously without async 9p I/O.
    pub(crate) fn seed_host_file_content(&mut self, path: &str, content: Vec<u8>) {
        if let Some(node) = self.nodes.iter_mut().find(|n| n.path == path) {
            node.contents = content;
            return;
        }
        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: path.to_owned(),
            kind: FsNodeKind::File,
            contents: content,
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
                contents: Vec::new(),
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
            contents: Vec::new(),
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

    fn host_service(
        &self,
    ) -> core::result::Result<HostFsService, fs_types::ErrorCode> {
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
        // Host paths are handled asynchronously in the WASI trait impl.
        assert!(
            self.host_path(path).is_none(),
            "stat on host path {path} must be handled in the async WASI impl"
        );
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
        // Host paths are handled asynchronously in the WASI trait impl;
        // reaching here with a host path indicates a caller forgot to route
        // to the async path.
        assert!(
            self.host_path(path).is_none(),
            "metadata_hash on host path {path} must be handled in the async WASI impl"
        );
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
        // `open_at`, so reading them here walks the same embedded node list
        // as guest-owned directories.
        assert!(
            self.host_path(path).is_none() || self.get_node(path).is_ok(),
            "read_directory on host path {path} requires prior eager seed via open_at"
        );
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
        // Host file contents are eagerly cached during `open_at`, so stream
        // reads always fall through to the embedded node list.
        assert!(
            self.host_path(&descriptor.path).is_none()
                || self.get_node(&descriptor.path).is_ok(),
            "read_file_chunk on host path {} requires prior eager seed via open_at",
            descriptor.path
        );
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
        if offset >= node.contents.len() {
            return Ok(Vec::new());
        }
        let end = offset
            .checked_add(max_bytes)
            .map(|value| value.min(node.contents.len()))
            .ok_or(fs_types::ErrorCode::Overflow)?;
        Ok(node.contents[offset..end].to_vec())
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
        if node.contents.len() < offset {
            node.contents.resize(offset, 0);
        }
        if node.contents.len() < end {
            node.contents.resize(end, 0);
        }
        node.contents[offset..end].copy_from_slice(bytes);
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
        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let absolute = crate::resolve_child_path(&base.path, path)
            .map_err(map_component_fs_path_error)?;
        // Host-backed paths are handled in the async WASI trait impl.
        assert!(
            self.host_path(&absolute).is_none(),
            "open_at on host path {absolute} must be handled in the async WASI impl"
        );

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
                existing.contents.clear();
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
            contents: Vec::new(),
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
        let absolute = crate::resolve_child_path(&base.path, path)
            .map_err(map_component_fs_path_error)?;
        assert!(
            self.host_path(&absolute).is_none(),
            "remove_directory_at on host path {absolute} must be handled in the async WASI impl"
        );
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
        let absolute = crate::resolve_child_path(&base.path, path)
            .map_err(map_component_fs_path_error)?;
        assert!(
            self.host_path(&absolute).is_none(),
            "create_directory_at on host path {absolute} must be handled in the async WASI impl"
        );
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
            contents: Vec::new(),
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
        let absolute = crate::resolve_child_path(&base.path, path)
            .map_err(map_component_fs_path_error)?;
        assert!(
            self.host_path(&absolute).is_none(),
            "unlink_file_at on host path {absolute} must be handled in the async WASI impl"
        );
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
        assert!(
            self.host_path(&source_absolute).is_none()
                && self.host_path(&destination_absolute).is_none(),
            "rename_at on host paths must be handled in the async WASI impl"
        );

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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<wasi::clocks::monotonic_clock::Mark> {
        Ok(self.now_nanos())
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl<CpuImpl, HostFs> wasi::clocks::monotonic_clock::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn wait_until<T: Send>(
        accessor: &Accessor<T, Self>,
        when: wasi::clocks::monotonic_clock::Mark,
    ) -> Result<()> {
        while accessor.with(|mut access| access.get().now_nanos()) < when {
            yield_now().await;
        }
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let message = match status {
            Ok(()) => "guest requested wasi exit success",
            Err(()) => "guest requested wasi exit failure",
        };
        Err(wasmtime::Error::msg(message))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi exit code {status_code}"
        )))
    }
}

impl<CpuImpl, HostFs> wasi::cli::stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> wasi::cli::stdin::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream<T>(
        mut access: Access<'_, T, Self>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), cli_types::ErrorCode>>,
    )> {
        let stream = StreamReader::new(&mut access, Vec::<u8>::new())?;
        let future = FutureReader::new(&mut access, async {
            Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(()))
        })?;
        Ok((stream, future))
    }
}

impl<CpuImpl, HostFs> wasi::cli::stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> wasi::cli::stdout::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> wasi::cli::stderr::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}
impl<CpuImpl, HostFs> wasi::cli::terminal_input::HostTerminalInput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_output::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}
impl<CpuImpl, HostFs> wasi::cli::terminal_output::HostTerminalOutput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::random::random::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }
}

impl<CpuImpl, HostFs> wasi::filesystem::preopens::Host for StoreData<CpuImpl, HostFs>
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

impl<CpuImpl, HostFs> wasi::filesystem::types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn convert_error_code(&mut self, error: FsError) -> Result<fs_types::ErrorCode> {
        error.downcast()
    }
}
impl<CpuImpl, HostFs> wasi::filesystem::types::HostDescriptor for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, descriptor: Resource<FsDescriptor>) -> Result<()> {
        self.table.delete(descriptor)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::filesystem::types::HostDescriptorWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
        accessor.with(|mut access| {
            access.get().filesystem().stat(&path).map_err(Into::into)
        })
    }

    async fn stat_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported.into());
        }
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
            access.get().filesystem().stat(&absolute).map_err(Into::into)
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

        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported.into());
        }
        if base_kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory.into());
        }

        let absolute = crate::resolve_child_path(&base_path, &path)
            .map_err(map_component_fs_path_error)?;
        let host_path = crate::guest_host_share_path(&absolute).map(|p| p.to_owned());

        if let Some(host_path) = host_path {
            // Host filesystem path — do async I/O outside accessor.
            let service = accessor.with(|mut access| {
                access.get().filesystem().host_service().map_err(FsError::from)
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
        let (source_absolute, destination_absolute, source_flags, destination_flags) =
            accessor.with(|mut access| {
                let source_base = get_fs_descriptor(access.get(), &source_descriptor)?;
                let destination_base =
                    get_fs_descriptor(access.get(), &destination_descriptor)?;
                let source_absolute =
                    crate::resolve_child_path(&source_base.path, &source_path)
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
        let source_host =
            crate::guest_host_share_path(&source_absolute).map(|p| p.to_owned());
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
        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported.into());
        }
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}
impl<CpuImpl, HostFs> wasi::sockets::types::HostTcpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn bind(
        &mut self,
        _: Resource<UdpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    async fn connect(
        &mut self,
        _: Resource<UdpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn create(
        &mut self,
        _: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<UdpSocket>, socket_types::ErrorCode>> {
        Ok(Err(socket_types::ErrorCode::NotSupported))
    }

    fn disconnect(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_local_address(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_remote_address(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_address_family(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_unicast_hop_limit(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn set_unicast_hop_limit(
        &mut self,
        _: Resource<UdpSocket>,
        _: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_receive_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn set_receive_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_send_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn set_send_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn drop(&mut self, resource: Resource<UdpSocket>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::HostTcpSocketWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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

impl<CpuImpl, HostFs> wasi::sockets::types::HostUdpSocketWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn send<T>(
        _: &Accessor<T, Self>,
        _: Resource<UdpSocket>,
        _: Vec<u8>,
        _: Option<socket_types::IpSocketAddress>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    async fn receive<T>(
        _: &Accessor<T, Self>,
        _: Resource<UdpSocket>,
    ) -> Result<
        core::result::Result<(Vec<u8>, socket_types::IpSocketAddress), socket_types::ErrorCode>,
    > {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }
}

impl<CpuImpl, HostFs> wasi::sockets::ip_name_lookup::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{}

impl<CpuImpl, HostFs> wasi::sockets::ip_name_lookup::HostWithStore for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
        wasmtime::component::ResourceTableError::Full => {
            crate::ComponentResourceTableError::Full
        }
    };
    match crate::map_resource_table_error(mapped) {
        crate::ComponentFsResourceError::BadDescriptor => {
            fs_types::ErrorCode::BadDescriptor
        }
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

fn map_host_fs_error(error: crate::HostFsError) -> fs_types::ErrorCode {
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
