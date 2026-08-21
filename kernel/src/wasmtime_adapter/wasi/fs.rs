use super::*;

pub(super) struct ComponentFsProfile<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    pub(super) cpu: CpuImpl,
    pub(super) started_ticks: u64,
}

pub(super) fn component_fs_profile<CpuImpl, HostFs>(
    store: &StoreData<CpuImpl, HostFs>,
) -> Option<ComponentFsProfile<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store
        .runtime_state
        .profiling_enabled()
        .then(|| ComponentFsProfile {
            runtime_state: store.runtime_state.clone(),
            cpu: store.cpu.clone(),
            started_ticks: store.cpu.now().ticks(),
        })
}

pub(super) fn record_component_fs_profile<CpuImpl, HostFs>(
    profile: Option<ComponentFsProfile<CpuImpl, HostFs>>,
    operation: &'static str,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(profile) = profile {
        profile.runtime_state.record_profile_stack_parts(
            crate::ProfileScope::Kernel,
            "kernel;component-fs;",
            operation,
            profile
                .cpu
                .now()
                .ticks()
                .saturating_sub(profile.started_ticks),
        );
    }
}

#[derive(Clone, Debug)]
pub(super) struct FsNode {
    pub(super) path: String,
    pub(super) kind: FsNodeKind,
    pub(super) contents: Bytes,
    pub(super) identity: ObjectIdentity,
    pub(super) access_nanos: u64,
    pub(super) modified_nanos: u64,
    pub(super) status_nanos: u64,
    pub(super) readonly: bool,
    pub(super) link_count: u64,
}

#[derive(Debug)]
pub(super) struct DebugFileSystemState {
    pub(super) nodes: Vec<FsNode>,
    pub(super) path_index: HashMap<String, usize>,
    pub(super) next_inode: u64,
}

#[derive(Clone)]
pub(crate) struct DebugFileSystemSnapshot {
    pub(super) inner: Arc<Mutex<DebugFileSystemState>>,
}

impl core::fmt::Debug for DebugFileSystemSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DebugFileSystemSnapshot")
            .finish_non_exhaustive()
    }
}

impl FsNode {
    pub(super) fn new(
        path: String,
        kind: FsNodeKind,
        contents: Bytes,
        identity: ObjectIdentity,
        timestamp_nanos: u64,
        readonly: bool,
    ) -> Self {
        Self {
            path,
            kind,
            contents,
            identity,
            access_nanos: timestamp_nanos,
            modified_nanos: timestamp_nanos,
            status_nanos: timestamp_nanos,
            readonly,
            link_count: 1,
        }
    }

    pub(super) fn touch_status(&mut self, now_nanos: u64) {
        self.status_nanos = now_nanos;
    }

    pub(super) fn touch_modified(&mut self, now_nanos: u64) {
        self.modified_nanos = now_nanos;
        self.status_nanos = now_nanos;
    }

    pub(super) fn set_times(
        &mut self,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        status_nanos: u64,
    ) {
        if let Some(access_nanos) = access_nanos {
            self.access_nanos = access_nanos;
        }
        if let Some(modified_nanos) = modified_nanos {
            self.modified_nanos = modified_nanos;
        }
        if access_nanos.is_some() || modified_nanos.is_some() {
            self.status_nanos = status_nanos;
        }
    }
}

impl DebugFileSystemState {
    pub(super) fn with_root() -> Self {
        let mut state = Self {
            nodes: Vec::new(),
            path_index: HashMap::new(),
            next_inode: 2,
        };
        state.insert_node(FsNode::new(
            String::from("/"),
            FsNodeKind::Directory,
            Bytes::new(),
            ObjectIdentity::new(AuthorityDomain::GUEST_BOOTFS, 1),
            0,
            false,
        ));
        state
    }

    pub(super) fn insert_node(&mut self, node: FsNode) {
        let path = node.path.clone();
        let index = self.nodes.len();
        assert!(
            self.path_index.insert(path.clone(), index).is_none(),
            "debug filesystem path {path} already exists"
        );
        self.nodes.push(node);
    }

    pub(super) fn node_index(&self, path: &str) -> Option<usize> {
        self.path_index.get(path).copied()
    }

    pub(super) fn node(&self, path: &str) -> Option<&FsNode> {
        self.node_index(path).map(|index| &self.nodes[index])
    }

    pub(super) fn node_mut(&mut self, path: &str) -> Option<&mut FsNode> {
        let index = self.node_index(path)?;
        Some(&mut self.nodes[index])
    }

    pub(super) fn remove_node(&mut self, path: &str) -> Option<FsNode> {
        let index = self.path_index.remove(path)?;
        let removed = self.nodes.swap_remove(index);
        if index < self.nodes.len() {
            let moved_path = self.nodes[index].path.clone();
            let previous = self.path_index.insert(moved_path.clone(), index);
            assert!(
                previous.is_some(),
                "debug filesystem index lost moved path {moved_path}"
            );
        }
        Some(removed)
    }

    pub(super) fn retain_nodes(&mut self, mut keep: impl FnMut(&FsNode) -> bool) {
        self.nodes.retain(|node| keep(node));
        self.rebuild_path_index();
    }

    pub(super) fn rebuild_path_index(&mut self) {
        self.path_index.clear();
        for (index, node) in self.nodes.iter().enumerate() {
            assert!(
                self.path_index.insert(node.path.clone(), index).is_none(),
                "debug filesystem duplicate path {}",
                node.path
            );
        }
    }
}

#[derive(Clone)]
pub struct FsDescriptor {
    pub(crate) path: String,
    pub(crate) kind: FsNodeKind,
    pub(crate) flags: fs_types::DescriptorFlags,
    pub(crate) identity: Option<ObjectIdentity>,
}

pub(super) fn embedded_absolute_path(relative: &str) -> String {
    let mut path = String::with_capacity(relative.len() + 1);
    path.push('/');
    path.push_str(relative);
    path
}

pub(super) fn join_embedded_child(parent: &str, child: &str) -> String {
    let slash = usize::from(parent != "/");
    let mut path = String::with_capacity(parent.len() + slash + child.len());
    path.push_str(parent);
    if parent != "/" {
        path.push('/');
    }
    path.push_str(child);
    path
}

pub(super) fn append_path_suffix(path: &str, suffix: &str) -> String {
    let mut combined = String::with_capacity(path.len() + suffix.len());
    combined.push_str(path);
    combined.push_str(suffix);
    combined
}

pub(crate) fn resolve_symlink_payload(
    link_path: &str,
    payload: &str,
) -> core::result::Result<String, fs_types::ErrorCode> {
    if payload.is_empty() || payload.starts_with('/') {
        return Err(fs_types::ErrorCode::NotPermitted);
    }
    if payload.split('/').any(|segment| segment == "..") {
        return Err(fs_types::ErrorCode::NotPermitted);
    }
    let parent = crate::parent_path(link_path);
    crate::resolve_child_path(parent, payload).map_err(map_component_fs_path_error)
}

pub(crate) struct DebugFileSystem<State, HostFsService> {
    pub(super) snapshot: DebugFileSystemSnapshot,
    pub(super) runtime_state: State,
    pub(super) _host_fs: PhantomData<HostFsService>,
}

pub(super) enum FileWriteMode {
    At(usize),
    Append,
}

pub(super) enum FsWriteOffset {
    At(usize),
    Append,
}

/// A host-share file a stream is bound to.
///
/// Streams carry their own service handle and host-relative path so each
/// chunk can be driven from a `poll` context without re-entering the store.
#[derive(Clone)]
pub(crate) struct HostFileStreamTarget<HostFs>
where
    HostFs: crate::HostFileSystem,
{
    pub(crate) service: HostFs,
    pub(crate) path: String,
}

impl<HostFs> HostFileStreamTarget<HostFs>
where
    HostFs: crate::HostFileSystem,
{
    /// Binds a descriptor to the host share when its path lives under the
    /// host mount, otherwise reports that the embedded filesystem owns it.
    pub(crate) fn for_descriptor<CpuImpl>(
        store: &StoreData<CpuImpl, HostFs>,
        descriptor: &FsDescriptor,
    ) -> core::result::Result<Option<Self>, fs_types::ErrorCode>
    where
        CpuImpl: Cpu + Clone,
    {
        let Some(path) = crate::guest_host_share_path(&descriptor.path) else {
            return Ok(None);
        };
        Ok(Some(Self {
            service: store.filesystem().host_service()?,
            path: path.to_owned(),
        }))
    }
}

/// A 9p transfer a file stream started and is now waiting on.
type PendingHostTransfer<T> =
    Pin<Box<dyn core::future::Future<Output = core::result::Result<T, fs_types::ErrorCode>> + Send>>;

pub(super) struct FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    pub(super) descriptor: FsDescriptor,
    pub(super) mode: FileWriteMode,
    pub(super) host: Option<HostFileStreamTarget<HostFs>>,
    pub(super) pending: Option<PendingHostTransfer<usize>>,
    pub(super) result: Option<oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>>,
}

pub(super) struct FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    pub(super) descriptor: FsDescriptor,
    pub(super) offset: u64,
    pub(super) chunk_bytes: usize,
    pub(super) host: Option<HostFileStreamTarget<HostFs>>,
    pub(super) pending: Option<PendingHostTransfer<Vec<u8>>>,
    pub(super) result: Option<oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>>,
}

// The in-flight 9p transfer is already heap-pinned, and nothing else in
// either stream is address-sensitive, so both drive their pending future
// directly from `Pin<&mut Self>`.
impl<T, CpuImpl, HostFs> Unpin for FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<T, CpuImpl, HostFs> Unpin for FileWriteConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<T, CpuImpl, HostFs> FileReadStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        offset: u64,
        chunk_bytes: usize,
        host: Option<HostFileStreamTarget<HostFs>>,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            offset,
            chunk_bytes,
            host,
            pending: None,
            result: Some(result),
        }
    }

    pub(super) fn complete(&mut self, result: core::result::Result<(), fs_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }

    /// Starts the next bounded host read.
    ///
    /// Only `chunk_bytes` are ever in flight, so a stream over a multi-gigabyte
    /// host file never holds more than one chunk of kernel memory.
    fn start_host_read(&mut self, capacity: Option<usize>) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let request = capacity.unwrap_or(self.chunk_bytes).min(self.chunk_bytes);
        let max_bytes = u32::try_from(request).unwrap_or(u32::MAX);
        let offset = self.offset;
        self.pending = Some(Box::pin(async move {
            host.service
                .read_file_range(&host.path, offset, max_bytes)
                .await
                .map_err(map_host_fs_error)
        }));
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
    type Buffer = BytesStreamBuffer;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            self.complete(Ok(()));
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        if self.host.is_some() {
            if self.descriptor.kind != FsNodeKind::File {
                self.complete(Err(fs_types::ErrorCode::IsDirectory));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            if !self.descriptor.flags.contains(fs_types::DescriptorFlags::READ) {
                self.complete(Err(fs_types::ErrorCode::NotPermitted));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            loop {
                if self.pending.is_none() {
                    let capacity = destination.remaining(&mut store);
                    if capacity == Some(0) {
                        return Poll::Ready(Ok(StreamResult::Completed));
                    }
                    self.start_host_read(capacity);
                }
                let pending = self
                    .pending
                    .as_mut()
                    .expect("host read future must be present before polling");
                match pending.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(bytes)) if bytes.is_empty() => {
                        self.pending = None;
                        self.complete(Ok(()));
                        return Poll::Ready(Ok(StreamResult::Dropped));
                    }
                    Poll::Ready(Ok(bytes)) => {
                        self.pending = None;
                        self.offset = self
                            .offset
                            .checked_add(
                                u64::try_from(bytes.len()).expect("read chunk size overflowed u64"),
                            )
                            .ok_or_else(|| {
                                wasmtime::Error::new(WasiAdapterTrap::FileReadOffsetOverflow)
                            })?;
                        destination.set_buffer(BytesStreamBuffer::new(Bytes::from(bytes)));
                        return Poll::Ready(Ok(StreamResult::Completed));
                    }
                    Poll::Ready(Err(error)) => {
                        self.pending = None;
                        self.complete(Err(error));
                        return Poll::Ready(Ok(StreamResult::Dropped));
                    }
                }
            }
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
                    .ok_or_else(|| wasmtime::Error::new(WasiAdapterTrap::FileReadOffsetOverflow))?;
                destination.set_buffer(BytesStreamBuffer::new(bytes));
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
    pub(super) fn new_at(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        offset: usize,
        host: Option<HostFileStreamTarget<HostFs>>,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            mode: FileWriteMode::At(offset),
            host,
            pending: None,
            result: Some(result),
        }
    }

    pub(super) fn new_append(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        descriptor: FsDescriptor,
        host: Option<HostFileStreamTarget<HostFs>>,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            mode: FileWriteMode::Append,
            host,
            pending: None,
            result: Some(result),
        }
    }

    pub(super) fn complete(&mut self, result: core::result::Result<(), fs_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }

    /// Starts the 9p transfer for one batch of stream bytes.
    ///
    /// Positional writes carry the stream offset; append writes resolve the
    /// host's end of file inside the client so a concurrently grown file is
    /// never overwritten. Both resolve with the number of bytes handed over.
    fn start_host_write(&mut self, bytes: Vec<u8>) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let mode = match &self.mode {
            FileWriteMode::At(offset) => FileWriteMode::At(*offset),
            FileWriteMode::Append => FileWriteMode::Append,
        };
        self.pending = Some(Box::pin(async move {
            let written = bytes.len();
            match mode {
                FileWriteMode::At(offset) => {
                    let offset = u64::try_from(offset).expect("file offset overflowed u64");
                    host.service
                        .write_file(&host.path, offset, &bytes)
                        .await
                        .map_err(map_host_fs_error)?;
                }
                FileWriteMode::Append => {
                    host.service
                        .append_file(&host.path, &bytes)
                        .await
                        .map_err(map_host_fs_error)?;
                }
            }
            Ok(written)
        }));
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
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        if self.host.is_some() {
            if self.descriptor.kind != FsNodeKind::File {
                self.complete(Err(fs_types::ErrorCode::IsDirectory));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            if !self.descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                self.complete(Err(fs_types::ErrorCode::NotPermitted));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            loop {
                if self.pending.is_none() {
                    let available = source.remaining(&mut store);
                    if available == 0 {
                        return Poll::Ready(Ok(StreamResult::Completed));
                    }
                    let mut bytes = Vec::with_capacity(available);
                    source.read(&mut store, &mut bytes)?;
                    self.start_host_write(bytes);
                }
                let pending = self
                    .pending
                    .as_mut()
                    .expect("host write future must be present before polling");
                match pending.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(written)) => {
                        self.pending = None;
                        if let FileWriteMode::At(offset) = &mut self.mode {
                            *offset = offset
                                .checked_add(written)
                                .expect("file write offset overflowed usize");
                        }
                        return Poll::Ready(Ok(StreamResult::Completed));
                    }
                    Poll::Ready(Err(error)) => {
                        self.pending = None;
                        self.complete(Err(error));
                        return Poll::Ready(Ok(StreamResult::Dropped));
                    }
                }
            }
        }

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
            snapshot: DebugFileSystemSnapshot {
                inner: Arc::new(Mutex::new(DebugFileSystemState::with_root())),
            },
            runtime_state,
            _host_fs: PhantomData,
        };

        if let Some(bootfs) = filesystem.runtime_state.bootfs() {
            filesystem.seed_bootfs(bootfs);
        }
        if filesystem.runtime_state.host_filesystem_service().is_some() {
            filesystem.ensure_directory(crate::HOST_SHARE_GUEST_MOUNT_PATH, true);
        }

        filesystem
    }

    pub(crate) fn from_snapshot(runtime_state: State, snapshot: DebugFileSystemSnapshot) -> Self {
        Self {
            snapshot,
            runtime_state,
            _host_fs: PhantomData,
        }
    }

    pub(crate) fn snapshot(&self) -> DebugFileSystemSnapshot {
        self.snapshot.clone()
    }

    pub(crate) fn replace_with_snapshot(&mut self, snapshot: DebugFileSystemSnapshot) {
        self.snapshot = snapshot;
    }

    pub(super) fn seed_bootfs(&mut self, image: EmbeddedBootFs) {
        for directory in image.directories() {
            self.insert_bootfs_directory(directory);
        }
        for file in image.files() {
            self.insert_bootfs_file(file);
        }
    }

    pub(super) fn insert_bootfs_directory(&mut self, directory: &crate::EmbeddedBootDirectory) {
        let absolute = embedded_absolute_path(directory.path());
        let mut current = String::from("/");

        for segment in directory
            .path()
            .split('/')
            .filter(|segment| !segment.is_empty())
        {
            let next = join_embedded_child(&current, segment);
            if next == absolute {
                break;
            }

            self.ensure_directory(&next, true);
            current = next;
        }

        self.ensure_bootfs_directory(&absolute, directory.modified_nanos());
    }

    pub(super) fn insert_bootfs_file(&mut self, file: &crate::EmbeddedBootFile) {
        let absolute = embedded_absolute_path(file.path());
        let mut current = String::from("/");

        for segment in file.path().split('/').filter(|segment| !segment.is_empty()) {
            let next = join_embedded_child(&current, segment);

            if next == absolute {
                break;
            }

            self.ensure_directory(&next, true);
            current = next;
        }

        if self.get_node(&absolute).is_ok() {
            return;
        }

        let identity = self.allocate_identity();
        let modified_nanos = file.modified_nanos();
        self.snapshot.inner.lock().insert_node(FsNode::new(
            absolute,
            FsNodeKind::File,
            Bytes::from_static(file.contents()),
            identity,
            modified_nanos,
            true,
        ));
    }

    /// Drops the embedded nodes mirroring a host subtree.
    ///
    /// Only directory listings are mirrored, so this clears stale directory
    /// entries after a remove or rename rather than any cached file content.
    pub(crate) fn invalidate_host_subtree(&mut self, path: &str) {
        let mut state = self.snapshot.inner.lock();
        state.retain_nodes(|node| {
            node.path != path && !crate::path_is_within_directory(&node.path, path)
        });
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
            let child_path = append_path_suffix(&prefix, &entry.name);
            if self.get_node(&child_path).is_ok() {
                continue;
            }
            let identity = self.allocate_identity();
            self.snapshot.inner.lock().insert_node(FsNode::new(
                child_path,
                if entry.is_directory {
                    FsNodeKind::Directory
                } else {
                    FsNodeKind::File
                },
                Bytes::new(),
                identity,
                0,
                true,
            ));
        }
    }

    pub(super) fn ensure_directory(&mut self, path: &str, readonly: bool) {
        self.ensure_directory_with_timestamp(path, readonly, 0);
    }

    pub(super) fn ensure_bootfs_directory(&mut self, path: &str, modified_nanos: u64) {
        self.ensure_directory_with_timestamp(path, true, modified_nanos);
    }

    pub(super) fn ensure_directory_with_timestamp(
        &mut self,
        path: &str,
        readonly: bool,
        timestamp_nanos: u64,
    ) {
        if path == "/" {
            return;
        }

        let mut state = self.snapshot.inner.lock();
        if let Some(existing) = state.node_mut(path) {
            assert!(
                existing.kind == FsNodeKind::Directory,
                "bootfs directory path {} collided with an existing file",
                path
            );
            existing.readonly |= readonly;
            if timestamp_nanos != 0 {
                existing.access_nanos = timestamp_nanos;
                existing.modified_nanos = timestamp_nanos;
                existing.status_nanos = timestamp_nanos;
            }
            return;
        }

        let local = state.next_inode;
        state.next_inode += 1;
        let identity = ObjectIdentity::new(AuthorityDomain::GUEST_BOOTFS, local);
        state.insert_node(FsNode::new(
            path.to_owned(),
            FsNodeKind::Directory,
            Bytes::new(),
            identity,
            timestamp_nanos,
            readonly,
        ));
    }

    pub(super) fn allocate_identity(&self) -> ObjectIdentity {
        let mut state = self.snapshot.inner.lock();
        let local = state.next_inode;
        state.next_inode += 1;
        ObjectIdentity::new(AuthorityDomain::GUEST_BOOTFS, local)
    }

    pub(super) fn node_size(node: &FsNode) -> u64 {
        match node.kind {
            FsNodeKind::Directory => 0,
            FsNodeKind::File | FsNodeKind::Symlink => node.contents.len() as u64,
        }
    }

    pub(super) fn descriptor_type(kind: FsNodeKind) -> fs_types::DescriptorType {
        descriptor_type_from_node_kind(kind)
    }

    pub(super) fn descriptor_identity(
        &self,
        descriptor: &FsDescriptor,
    ) -> core::result::Result<ObjectIdentity, fs_types::ErrorCode> {
        descriptor.identity.map_or_else(
            || self.get_node(&descriptor.path).map(|node| node.identity),
            Ok,
        )
    }

    pub(super) fn require_same_authority_domain(
        &self,
        left: &FsDescriptor,
        right: &FsDescriptor,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if self.descriptor_identity(left)?.domain() != self.descriptor_identity(right)?.domain() {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        Ok(())
    }

    pub(super) fn resolve_symlink_payload(
        link_path: &str,
        payload: &str,
    ) -> core::result::Result<String, fs_types::ErrorCode> {
        resolve_symlink_payload(link_path, payload)
    }

    pub(super) fn resolve_open_path(
        &self,
        absolute: &str,
        follow_symlink: bool,
        depth: usize,
    ) -> core::result::Result<String, fs_types::ErrorCode> {
        if depth > MAX_SYMLINK_DEPTH {
            return Err(fs_types::ErrorCode::Loop);
        }
        let node = self.get_node(absolute)?;
        if node.kind != FsNodeKind::Symlink || !follow_symlink {
            return Ok(absolute.to_owned());
        }
        let payload = core::str::from_utf8(&node.contents)
            .map_err(|_| fs_types::ErrorCode::IllegalByteSequence)?;
        let target = Self::resolve_symlink_payload(absolute, payload)?;
        self.resolve_open_path(&target, follow_symlink, depth + 1)
    }

    pub(super) fn host_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        crate::guest_host_share_path(path)
    }

    pub(crate) fn host_service(&self) -> core::result::Result<HostFsService, fs_types::ErrorCode> {
        self.runtime_state
            .host_filesystem_service()
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    #[cfg(test)]
    pub(crate) fn root_descriptor(&self) -> FsDescriptor {
        let root = self
            .get_node("/")
            .expect("debug filesystem root node must exist");
        FsDescriptor {
            path: String::from("/"),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: Some(root.identity),
        }
    }

    pub(super) fn get_node(&self, path: &str) -> core::result::Result<FsNode, fs_types::ErrorCode> {
        self.snapshot
            .inner
            .lock()
            .node(path)
            .cloned()
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    pub(super) fn with_node<R>(
        &self,
        path: &str,
        read: impl FnOnce(&FsNode) -> core::result::Result<R, fs_types::ErrorCode>,
    ) -> core::result::Result<R, fs_types::ErrorCode> {
        let state = self.snapshot.inner.lock();
        let node = state.node(path).ok_or(fs_types::ErrorCode::NoEntry)?;
        read(node)
    }

    pub(crate) fn stat(
        &self,
        path: &str,
    ) -> core::result::Result<fs_types::DescriptorStat, fs_types::ErrorCode> {
        // Host paths are answered asynchronously in the WASI trait impls,
        // which never reach here. What does reach here for a host path is a
        // directory node mirrored by `seed_host_directory_entries`; anything
        // else is genuinely absent and reports NoEntry rather than panicking.
        self.with_node(path, |node| {
            let size = Self::node_size(node);
            Ok(fs_types::DescriptorStat {
                type_: Self::descriptor_type(node.kind),
                link_count: node.link_count,
                size,
                data_access_timestamp: Some(system_time_from_nanos(node.access_nanos)),
                data_modification_timestamp: Some(system_time_from_nanos(node.modified_nanos)),
                status_change_timestamp: Some(system_time_from_nanos(node.status_nanos)),
            })
        })
    }

    /// Object identity of an embedded node, for surfaces that report
    /// `st_dev`/`st_ino` rather than a metadata hash.
    pub(crate) fn identity_at_path(
        &self,
        path: &str,
    ) -> core::result::Result<ObjectIdentity, fs_types::ErrorCode> {
        self.with_node(path, |node| Ok(node.identity))
    }

    pub(crate) fn metadata_hash(
        &self,
        path: &str,
    ) -> core::result::Result<fs_types::MetadataHashValue, fs_types::ErrorCode> {
        // Host paths are answered asynchronously in the WASI trait impls;
        // only mirrored host directories reach the embedded view here.
        self.with_node(path, |node| {
            let size = Self::node_size(node);
            Ok(metadata_hash_value(
                node.identity,
                node.modified_nanos ^ size,
            ))
        })
    }

    pub(crate) fn read_directory(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<fs_types::DirectoryEntry>, fs_types::ErrorCode> {
        // Opening a host directory seeds its direct children into the
        // embedded FS, so listing one walks the same node list as a
        // guest-owned directory. If nothing was seeded, report NoEntry.
        let node = self.get_node(path)?;
        if node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let mut entries = Vec::new();
        let state = self.snapshot.inner.lock();
        for child in &state.nodes {
            if child.path == path {
                continue;
            }
            let Some(remainder) = crate::strip_directory_prefix(&child.path, path) else {
                continue;
            };
            if let Some((name, _)) = remainder.split_once('/') {
                push_directory_entry_if_absent(
                    &mut entries,
                    fs_types::DescriptorType::Directory,
                    name,
                );
                continue;
            }
            entries.push(fs_types::DirectoryEntry {
                type_: Self::descriptor_type(child.kind),
                name: remainder.to_string(),
            });
        }
        drop(state);

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
    ) -> core::result::Result<Bytes, fs_types::ErrorCode> {
        // Host file contents are never cached here: a stream over a host
        // file carries its own `HostFileStreamTarget` and pulls bounded
        // chunks over 9p instead. This path serves embedded nodes only, and
        // a missing node is NoEntry.
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
            return Ok(Bytes::new());
        }
        let end = offset
            .checked_add(max_bytes)
            .map(|value| value.min(contents.len()))
            .ok_or(fs_types::ErrorCode::Overflow)?;
        Ok(node.contents.slice(offset..end))
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

        let mut state = self.snapshot.inner.lock();
        match state.node(path).cloned() {
            Some(node) => {
                if node.kind != FsNodeKind::File {
                    return Err(fs_types::ErrorCode::IsDirectory);
                }
                if node.readonly {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                let contents = Bytes::copy_from_slice(bytes);
                for linked in state
                    .nodes
                    .iter_mut()
                    .filter(|linked| linked.identity == node.identity)
                {
                    linked.contents = contents.clone();
                    linked.touch_modified(now_nanos);
                }
                Ok(())
            }
            None => {
                let parent = crate::parent_path(path);
                let parent_node = state.node(parent).ok_or(fs_types::ErrorCode::NoEntry)?;
                if parent_node.kind != FsNodeKind::Directory {
                    return Err(fs_types::ErrorCode::NotDirectory);
                }
                if parent_node.readonly {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                let local = state.next_inode;
                state.next_inode += 1;
                let identity = ObjectIdentity::new(AuthorityDomain::GUEST_BOOTFS, local);
                state.insert_node(FsNode::new(
                    path.to_owned(),
                    FsNodeKind::File,
                    Bytes::copy_from_slice(bytes),
                    identity,
                    now_nanos,
                    false,
                ));
                Ok(())
            }
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
        self.write_at_or_append(descriptor, FsWriteOffset::At(offset), bytes, now_nanos)
    }

    pub(crate) fn write_at_with<Fill>(
        &mut self,
        descriptor: &FsDescriptor,
        offset: usize,
        byte_len: usize,
        now_nanos: u64,
        fill: Fill,
    ) -> core::result::Result<(), fs_types::ErrorCode>
    where
        Fill: FnOnce(&mut [u8]),
    {
        self.write_at_or_append_with(
            descriptor,
            FsWriteOffset::At(offset),
            byte_len,
            now_nanos,
            fill,
        )
    }

    pub(super) fn write_at_or_append(
        &mut self,
        descriptor: &FsDescriptor,
        offset: FsWriteOffset,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        self.write_at_or_append_with(descriptor, offset, bytes.len(), now_nanos, |destination| {
            destination.copy_from_slice(bytes)
        })
    }

    pub(super) fn write_at_or_append_with<Fill>(
        &mut self,
        descriptor: &FsDescriptor,
        offset: FsWriteOffset,
        byte_len: usize,
        now_nanos: u64,
        fill: Fill,
    ) -> core::result::Result<(), fs_types::ErrorCode>
    where
        Fill: FnOnce(&mut [u8]),
    {
        // This is the synchronous embedded-filesystem writer. Host-share
        // writes go through the 9p client from the async WASI entry points
        // and the stream consumer, both of which branch on the path before
        // calling here; reaching this with a host path would mean a caller
        // bypassed that routing.
        if self.host_path(&descriptor.path).is_some() {
            return Err(fs_types::ErrorCode::Unsupported);
        }

        let mut state = self.snapshot.inner.lock();
        let Some(index) = state.node_index(&descriptor.path) else {
            return Err(fs_types::ErrorCode::NoEntry);
        };
        let node = &state.nodes[index];
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let identity = node.identity;
        let linked_count = node.link_count;
        let offset = match offset {
            FsWriteOffset::At(offset) => offset,
            FsWriteOffset::Append => node.contents.len(),
        };
        let end = offset
            .checked_add(byte_len)
            .ok_or(fs_types::ErrorCode::Overflow)?;
        let contents = core::mem::take(&mut state.nodes[index].contents);
        let mut contents = if linked_count == 1 {
            match contents.try_into_mut() {
                Ok(contents) => contents,
                Err(contents) => BytesMut::from(contents.as_ref()),
            }
        } else {
            BytesMut::from(contents.as_ref())
        };
        if contents.len() < offset {
            contents.resize(offset, 0);
        }
        if contents.len() < end {
            contents.resize(end, 0);
        }
        fill(&mut contents[offset..end]);
        let contents = contents.freeze();
        for linked in state
            .nodes
            .iter_mut()
            .filter(|node| node.identity == identity)
        {
            linked.contents = contents.clone();
            linked.touch_modified(now_nanos);
        }
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        descriptor: &FsDescriptor,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        self.write_at_or_append(descriptor, FsWriteOffset::Append, bytes, now_nanos)
    }

    pub(crate) fn append_with<Fill>(
        &mut self,
        descriptor: &FsDescriptor,
        byte_len: usize,
        now_nanos: u64,
        fill: Fill,
    ) -> core::result::Result<(), fs_types::ErrorCode>
    where
        Fill: FnOnce(&mut [u8]),
    {
        self.write_at_or_append_with(descriptor, FsWriteOffset::Append, byte_len, now_nanos, fill)
    }

    pub(crate) fn set_size(
        &mut self,
        descriptor: &FsDescriptor,
        size: u64,
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if self.host_path(&descriptor.path).is_some() {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let size: usize = size.try_into().map_err(|_| fs_types::ErrorCode::Overflow)?;
        let mut state = self.snapshot.inner.lock();
        let Some(index) = state.node_index(&descriptor.path) else {
            return Err(fs_types::ErrorCode::NoEntry);
        };
        let node = &state.nodes[index];
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let identity = node.identity;
        let linked_count = node.link_count;
        let contents = core::mem::take(&mut state.nodes[index].contents);
        let mut contents = if linked_count == 1 {
            match contents.try_into_mut() {
                Ok(contents) => contents,
                Err(contents) => BytesMut::from(contents.as_ref()),
            }
        } else {
            BytesMut::from(contents.as_ref())
        };
        contents.resize(size, 0);
        let contents = contents.freeze();
        for linked in state
            .nodes
            .iter_mut()
            .filter(|node| node.identity == identity)
        {
            linked.contents = contents.clone();
            linked.touch_modified(now_nanos);
        }
        Ok(())
    }

    pub(crate) fn set_times_at_path(
        &mut self,
        path: &str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        status_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if self.host_path(path).is_some() {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        let node = self.get_node(path)?;
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let identity = node.identity;
        for linked in self
            .snapshot
            .inner
            .lock()
            .nodes
            .iter_mut()
            .filter(|node| node.identity == identity)
        {
            linked.set_times(access_nanos, modified_nanos, status_nanos);
        }
        Ok(())
    }

    pub(crate) fn set_times(
        &mut self,
        descriptor: &FsDescriptor,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        status_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        self.set_times_at_path(&descriptor.path, access_nanos, modified_nanos, status_nanos)
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
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        let follow_symlink = path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW);
        let absolute = if self.get_node(&absolute).is_ok() {
            self.resolve_open_path(&absolute, follow_symlink, 0)?
        } else {
            absolute
        };
        // Host-backed paths are handled in the async WASI trait impl;
        // this embedded path only handles paths that already have a
        // node (either genuinely local or previously seeded from the
        // host mount).
        let existing = self.get_node(&absolute).ok();
        if let Some(existing) = existing {
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
            if existing.kind == FsNodeKind::Symlink && follow_symlink {
                return Err(fs_types::ErrorCode::Loop);
            }
            let descriptor_flags =
                effective_open_descriptor_flags(base.flags, descriptor_flags, existing.kind)?;
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
                let identity = existing.identity;
                for linked in self
                    .snapshot
                    .inner
                    .lock()
                    .nodes
                    .iter_mut()
                    .filter(|node| node.identity == identity)
                {
                    linked.contents = Bytes::new();
                    linked.touch_modified(now_nanos);
                }
            }
            return Ok(FsDescriptor {
                path: absolute,
                kind: existing.kind,
                flags: descriptor_flags,
                identity: Some(existing.identity),
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
        let descriptor_flags =
            effective_open_descriptor_flags(base.flags, descriptor_flags, FsNodeKind::File)?;

        let parent = crate::parent_path(&absolute);
        let parent_node = self.get_node(parent)?;
        if parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let identity = self.allocate_identity();
        let node = FsNode::new(
            absolute.clone(),
            FsNodeKind::File,
            Bytes::new(),
            identity,
            now_nanos,
            false,
        );
        self.snapshot.inner.lock().insert_node(node);
        Ok(FsDescriptor {
            path: absolute,
            kind: FsNodeKind::File,
            flags: descriptor_flags,
            identity: Some(identity),
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
        if self
            .snapshot
            .inner
            .lock()
            .nodes
            .iter()
            .any(|child| crate::path_is_within_directory(&child.path, &absolute))
        {
            return Err(fs_types::ErrorCode::NotEmpty);
        }
        let identity = node.identity;
        let mut state = self.snapshot.inner.lock();
        if state.remove_node(&absolute).is_some() {
            for linked in state
                .nodes
                .iter_mut()
                .filter(|node| node.identity == identity)
            {
                linked.link_count = linked
                    .link_count
                    .checked_sub(1)
                    .expect("filesystem hardlink count underflow");
            }
        }
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

        let identity = self.allocate_identity();
        self.snapshot.inner.lock().insert_node(FsNode::new(
            absolute,
            FsNodeKind::Directory,
            Bytes::new(),
            identity,
            now_nanos,
            false,
        ));
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
        if node.kind == FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        self.snapshot.inner.lock().remove_node(&absolute);
        Ok(())
    }

    pub(crate) fn link_at(
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
        if !destination_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        self.require_same_authority_domain(source_base, destination_base)?;

        let source_absolute = crate::resolve_child_path(&source_base.path, source_path)
            .map_err(map_component_fs_path_error)?;
        let destination_absolute =
            crate::resolve_child_path(&destination_base.path, destination_path)
                .map_err(map_component_fs_path_error)?;
        if source_absolute == "/" || destination_absolute == "/" {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        if self.get_node(&destination_absolute).is_ok() {
            return Err(fs_types::ErrorCode::Exist);
        }

        let source_node = self.get_node(&source_absolute)?;
        if source_node.kind == FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        let destination_parent = crate::parent_path(&destination_absolute);
        let destination_parent_node = self.get_node(destination_parent)?;
        if destination_parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if destination_parent_node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let identity = source_node.identity;
        let link_count = source_node
            .link_count
            .checked_add(1)
            .expect("filesystem hardlink count overflow");
        let mut linked = source_node.clone();
        linked.path = destination_absolute;
        linked.link_count = link_count;
        linked.touch_status(now_nanos);
        let mut state = self.snapshot.inner.lock();
        for node in state
            .nodes
            .iter_mut()
            .filter(|node| node.identity == identity)
        {
            node.link_count = link_count;
            node.touch_status(now_nanos);
        }
        state.insert_node(linked);
        Ok(())
    }

    pub(crate) fn symlink_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
        payload: &str,
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        Self::resolve_symlink_payload(&absolute, payload)?;
        if self.get_node(&absolute).is_ok() {
            return Err(fs_types::ErrorCode::Exist);
        }
        let parent = crate::parent_path(&absolute);
        let parent_node = self.get_node(parent)?;
        if parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if parent_node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let identity = self.allocate_identity();
        self.snapshot.inner.lock().insert_node(FsNode::new(
            absolute,
            FsNodeKind::Symlink,
            Bytes::copy_from_slice(payload.as_bytes()),
            identity,
            now_nanos,
            false,
        ));
        Ok(())
    }

    pub(crate) fn readlink_at(
        &self,
        base: &FsDescriptor,
        path: &str,
    ) -> core::result::Result<String, fs_types::ErrorCode> {
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        let absolute =
            crate::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        let node = self.get_node(&absolute)?;
        if node.kind != FsNodeKind::Symlink {
            return Err(fs_types::ErrorCode::Invalid);
        }
        let payload = core::str::from_utf8(&node.contents)
            .map_err(|_| fs_types::ErrorCode::IllegalByteSequence)?;
        Ok(payload.to_owned())
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
        self.require_same_authority_domain(source_base, destination_base)?;
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
            if destination_absolute == source_absolute
                || crate::path_is_within_directory(&destination_absolute, &source_absolute)
            {
                return Err(fs_types::ErrorCode::NotPermitted);
            }
        }

        let mut state = self.snapshot.inner.lock();
        for node in &mut state.nodes {
            if node.path == source_absolute {
                node.path = destination_absolute.clone();
                node.touch_status(now_nanos);
                continue;
            }

            if source_node.kind == FsNodeKind::Directory
                && crate::path_is_within_directory(&node.path, &source_absolute)
            {
                node.path =
                    append_path_suffix(&destination_absolute, &node.path[source_absolute.len()..]);
                node.touch_status(now_nanos);
            }
        }
        state.rebuild_path_index();

        Ok(())
    }
}

pub(crate) fn preopen_descriptor(preopen: &crate::DirectoryPreopen) -> FsDescriptor {
    FsDescriptor {
        path: preopen.source_path().to_owned(),
        kind: FsNodeKind::Directory,
        flags: descriptor_flags_from_authority(preopen.rights()),
        identity: None,
    }
}

/// Node kind of a host-share object, from its 9p qid type bits.
pub(crate) fn host_metadata_node_kind(metadata: &crate::HostMetadata) -> FsNodeKind {
    if metadata.qid_type & P9_QID_TYPE_DIRECTORY != 0 {
        FsNodeKind::Directory
    } else {
        FsNodeKind::File
    }
}

/// Builds a WASI `descriptor-stat` from host-share metadata.
///
/// Timestamps and link count come from the host's own `Rgetattr`, so a guest
/// sees the same mtime the host does. Object identity is not part of
/// `descriptor-stat`; p2/p3 expose it through `metadata-hash` and preview1
/// through `st_dev`/`st_ino`, both derived from
/// [`crate::HostMetadata::identity`].
pub(crate) fn descriptor_stat_from_host_metadata(
    metadata: &crate::HostMetadata,
) -> fs_types::DescriptorStat {
    fs_types::DescriptorStat {
        type_: descriptor_type_from_node_kind(host_metadata_node_kind(metadata)),
        link_count: metadata.link_count,
        size: metadata.size,
        data_access_timestamp: Some(system_time_from_nanos(metadata.access_nanos)),
        data_modification_timestamp: Some(system_time_from_nanos(metadata.modified_nanos)),
        status_change_timestamp: Some(system_time_from_nanos(metadata.status_nanos)),
    }
}

pub(crate) fn descriptor_type_from_node_kind(kind: FsNodeKind) -> fs_types::DescriptorType {
    match kind {
        FsNodeKind::Directory => fs_types::DescriptorType::Directory,
        FsNodeKind::File => fs_types::DescriptorType::RegularFile,
        FsNodeKind::Symlink => fs_types::DescriptorType::SymbolicLink,
    }
}

pub(crate) fn metadata_hash_value(
    identity: ObjectIdentity,
    upper_entropy: u64,
) -> fs_types::MetadataHashValue {
    fs_types::MetadataHashValue {
        lower: identity.local(),
        upper: identity.domain().raw() ^ upper_entropy,
    }
}

pub(super) fn descriptor_flags_from_authority(
    rights: crate::DirectoryAuthorityRights,
) -> fs_types::DescriptorFlags {
    let mut flags = fs_types::DescriptorFlags::empty();
    if rights.contains(crate::DirectoryAuthorityRights::READ) {
        flags |= fs_types::DescriptorFlags::READ;
    }
    if rights.contains(crate::DirectoryAuthorityRights::WRITE) {
        flags |= fs_types::DescriptorFlags::WRITE;
    }
    if rights.contains(crate::DirectoryAuthorityRights::MUTATE_DIRECTORY) {
        flags |= fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    }
    flags
}

pub(super) fn push_directory_entry_if_absent(
    entries: &mut Vec<fs_types::DirectoryEntry>,
    type_: fs_types::DescriptorType,
    name: &str,
) {
    if entries.iter().any(|entry| entry.name == name) {
        return;
    }
    entries.push(fs_types::DirectoryEntry {
        type_,
        name: name.to_string(),
    });
}

pub(crate) fn validate_descriptor_flags_within_base(
    base_flags: fs_types::DescriptorFlags,
    requested: fs_types::DescriptorFlags,
) -> core::result::Result<(), fs_types::ErrorCode> {
    let authority_flags = fs_types::DescriptorFlags::READ
        | fs_types::DescriptorFlags::WRITE
        | fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    if !base_flags.contains(requested & authority_flags) {
        return Err(fs_types::ErrorCode::ReadOnly);
    }
    Ok(())
}

pub(crate) fn effective_open_descriptor_flags(
    base_flags: fs_types::DescriptorFlags,
    requested: fs_types::DescriptorFlags,
    kind: FsNodeKind,
) -> core::result::Result<fs_types::DescriptorFlags, fs_types::ErrorCode> {
    let mut effective = fs_types::DescriptorFlags::empty();
    if requested.contains(fs_types::DescriptorFlags::READ) {
        effective |= fs_types::DescriptorFlags::READ;
    }
    if requested.contains(fs_types::DescriptorFlags::WRITE) {
        effective |= fs_types::DescriptorFlags::WRITE;
    }
    if kind == FsNodeKind::Directory
        && requested.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
    {
        effective |= fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    }
    validate_descriptor_flags_within_base(base_flags, effective)?;
    Ok(effective)
}

impl<CpuImpl, HostFs> wasi::filesystem::preopens::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_directories(&mut self) -> Result<Vec<(Resource<FsDescriptor>, String)>> {
        let preopens = self.process_authority().directory_preopens().to_vec();
        preopens
            .iter()
            .map(|preopen| {
                let descriptor = preopen_descriptor(preopen);
                let resource = self.table.push(descriptor)?;
                Ok((resource, preopen.guest_name().to_owned()))
            })
            .collect()
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

impl<CpuImpl, HostFs, U> wasi::filesystem::types::HostDescriptorWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream(
        mut accessor: Access<'_, U, Self>,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), fs_types::ErrorCode>>,
    )> {
        let getter = accessor.getter();
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor)?;
        let host = HostFileStreamTarget::for_descriptor(accessor.get(), &descriptor)?;
        let (tx, rx) = oneshot::channel();
        let stream = StreamReader::new(
            &mut accessor,
            FileReadStreamProducer::new(
                getter,
                descriptor,
                offset,
                FILE_READ_CHUNK_BYTES,
                host,
                tx,
            ),
        )?;
        let future = FutureReader::new(&mut accessor, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(())),
            }
        })
        .map_err(FsError::trap)?;
        Ok((stream, future))
    }

    fn write_via_stream(
        mut accessor: Access<'_, U, Self>,
        descriptor: Resource<FsDescriptor>,
        mut data: StreamReader<u8>,
        offset: u64,
    ) -> Result<FutureReader<core::result::Result<(), fs_types::ErrorCode>>> {
        let offset: usize = offset
            .try_into()
            .map_err(|_| wasmtime::Error::new(WasiAdapterTrap::FileWriteOffsetOverflow))?;
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor).and_then(|descriptor| {
            let host = HostFileStreamTarget::for_descriptor(accessor.get(), &descriptor)?;
            Ok((descriptor, host))
        });
        let getter = accessor.getter();
        match descriptor {
            Ok((descriptor, host)) => {
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut accessor,
                    FileWriteConsumer::new_at(getter, descriptor, offset, host, tx),
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

    fn append_via_stream(
        mut accessor: Access<'_, U, Self>,
        descriptor: Resource<FsDescriptor>,
        mut data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), fs_types::ErrorCode>>> {
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor).and_then(|descriptor| {
            let host = HostFileStreamTarget::for_descriptor(accessor.get(), &descriptor)?;
            Ok((descriptor, host))
        });
        let getter = accessor.getter();
        match descriptor {
            Ok((descriptor, host)) => {
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut accessor,
                    FileWriteConsumer::new_append(getter, descriptor, host, tx),
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

    async fn advise(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: fs_types::Advice,
    ) -> Result<(), FsError> {
        accessor.with(|mut access| {
            get_fs_descriptor(access.get(), &descriptor)?;
            Ok(())
        })
    }

    async fn sync_data(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<(), FsError> {
        sync_descriptor(accessor, descriptor).await
    }

    async fn get_flags(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorFlags, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            Ok(descriptor.flags)
        })
    }

    async fn get_type(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorType, FsError> {
        let profile = accessor.with(|mut access| component_fs_profile(access.get()));
        let result = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let kind = match descriptor.kind {
                FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                FsNodeKind::File => fs_types::DescriptorType::RegularFile,
                FsNodeKind::Symlink => fs_types::DescriptorType::SymbolicLink,
            };
            Ok(kind)
        });
        record_component_fs_profile(profile, "get-type");
        result
    }

    async fn set_size(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        size: u64,
    ) -> Result<(), FsError> {
        let (path, flags) = accessor
            .with(|mut access| {
                let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
                Ok::<_, FsError>((descriptor.path.clone(), descriptor.flags))
            })
            .map_err(FsError::trap)?;
        if let Some(host_path) = crate::guest_host_share_path(&path).map(|p| p.to_owned()) {
            if !flags.contains(fs_types::DescriptorFlags::WRITE) {
                return Err(fs_types::ErrorCode::NotPermitted.into());
            }
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .set_file_size(&host_path, size)
                .await
                .map_err(map_host_fs_error)?;
            accessor.with(|mut access| {
                access.get().filesystem_mut().invalidate_host_subtree(&path);
            });
            return Ok(());
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let now_nanos = access.get().now_nanos();
            access
                .get()
                .filesystem_mut()
                .set_size(&descriptor, size, now_nanos)
                .map_err(Into::into)
        })
    }

    async fn set_times(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        data_access_timestamp: fs_types::NewTimestamp,
        data_modification_timestamp: fs_types::NewTimestamp,
    ) -> Result<(), FsError> {
        let (path, access_nanos, modified_nanos) = accessor
            .with(|mut access| {
                let now_nanos = access.get().system_time_nanos();
                let access_nanos = p3_new_timestamp_nanos(data_access_timestamp, now_nanos)?;
                let modified_nanos =
                    p3_new_timestamp_nanos(data_modification_timestamp, now_nanos)?;
                let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
                Ok::<_, FsError>((descriptor.path.clone(), access_nanos, modified_nanos))
            })
            .map_err(FsError::trap)?;
        if let Some(host_path) = crate::guest_host_share_path(&path).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .set_times(&host_path, access_nanos, modified_nanos)
                .await
                .map_err(map_host_fs_error)?;
            accessor.with(|mut access| {
                access.get().filesystem_mut().invalidate_host_subtree(&path);
            });
            return Ok(());
        }
        accessor.with(|mut access| {
            let now_nanos = access.get().system_time_nanos();
            access
                .get()
                .filesystem_mut()
                .set_times_at_path(&path, access_nanos, modified_nanos, now_nanos)
                .map_err(Into::into)
        })
    }

    fn read_directory(
        mut accessor: Access<'_, U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<(
        StreamReader<fs_types::DirectoryEntry>,
        FutureReader<core::result::Result<(), fs_types::ErrorCode>>,
    )> {
        let profile = component_fs_profile(accessor.get());
        let entries = {
            let descriptor = get_fs_descriptor(accessor.get(), &descriptor)?;
            accessor.get().filesystem().read_directory(&descriptor.path)
        };
        record_component_fs_profile(profile, "read-directory");
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

    async fn sync(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<(), FsError> {
        sync_descriptor(accessor, descriptor).await
    }

    async fn create_directory_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        let (absolute, base_flags, base_kind) = accessor
            .with(|mut access| {
                let base = get_fs_descriptor(access.get(), &descriptor)?;
                let absolute = crate::resolve_child_path(&base.path, &path)
                    .map_err(map_component_fs_path_error)?;
                Ok::<_, FsError>((absolute, base.flags, base.kind))
            })
            .map_err(FsError::trap)?;
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

    async fn stat(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        let profile = accessor.with(|mut access| component_fs_profile(access.get()));
        // Extract path from the descriptor synchronously, then do the
        // (potentially async) FS operation outside the accessor so the
        // kernel executor can drive the 9p transport concurrently.
        let path = accessor
            .with(|mut access| {
                let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
                Ok::<_, FsError>(descriptor.path.clone())
            })
            .map_err(FsError::trap)?;
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
            let result = descriptor_stat_from_host_metadata(&metadata);
            record_component_fs_profile(profile, "stat");
            return Ok(result);
        }
        let result =
            accessor.with(|mut access| access.get().filesystem().stat(&path).map_err(Into::into));
        record_component_fs_profile(profile, "stat");
        result
    }

    async fn stat_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        let profile = accessor.with(|mut access| component_fs_profile(access.get()));
        // Guest filesystem has no symlinks; SYMLINK_FOLLOW is a no-op.
        let _ = path_flags;
        let absolute = accessor
            .with(|mut access| {
                let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
                crate::resolve_child_path(&descriptor.path, &path)
                    .map_err(map_component_fs_path_error)
                    .map_err(FsError::from)
            })
            .map_err(FsError::trap)?;
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
            let result = descriptor_stat_from_host_metadata(&metadata);
            record_component_fs_profile(profile, "stat-at");
            return Ok(result);
        }
        let result = accessor.with(|mut access| {
            access
                .get()
                .filesystem()
                .stat(&absolute)
                .map_err(Into::into)
        });
        record_component_fs_profile(profile, "stat-at");
        result
    }

    async fn set_times_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        _: fs_types::PathFlags,
        path: String,
        data_access_timestamp: fs_types::NewTimestamp,
        data_modification_timestamp: fs_types::NewTimestamp,
    ) -> Result<(), FsError> {
        let (absolute, access_nanos, modified_nanos, now_nanos) = accessor
            .with(|mut access| {
                let now_nanos = access.get().system_time_nanos();
                let access_nanos = p3_new_timestamp_nanos(data_access_timestamp, now_nanos)?;
                let modified_nanos =
                    p3_new_timestamp_nanos(data_modification_timestamp, now_nanos)?;
                let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
                let absolute = crate::resolve_child_path(&descriptor.path, &path)
                    .map_err(map_component_fs_path_error)?;
                Ok::<_, FsError>((absolute, access_nanos, modified_nanos, now_nanos))
            })
            .map_err(FsError::trap)?;
        if let Some(host_path) = crate::guest_host_share_path(&absolute).map(|p| p.to_owned()) {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .set_times(&host_path, access_nanos, modified_nanos)
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
            access
                .get()
                .filesystem_mut()
                .set_times_at_path(&absolute, access_nanos, modified_nanos, now_nanos)
                .map_err(Into::into)
        })
    }

    async fn link_at(
        accessor: &Accessor<U, Self>,
        source_descriptor: Resource<FsDescriptor>,
        _: fs_types::PathFlags,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<(), FsError> {
        let allowed = accessor
            .with(|mut access| {
                let authority = access.get().process_authority();
                Ok::<_, wasmtime::Error>(
                    authority.derive_link_source_cap().is_ok()
                        && authority.derive_link_target_directory_cap().is_ok(),
                )
            })
            .map_err(FsError::trap)?;
        if !allowed {
            return Err(fs_types::ErrorCode::NotPermitted.into());
        }

        let (
            source_absolute,
            destination_absolute,
            source_kind,
            destination_kind,
            destination_flags,
        ) = accessor.with(|mut access| {
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
                source_base.kind,
                destination_base.kind,
                destination_base.flags,
            ))
        })?;
        if source_kind != FsNodeKind::Directory || destination_kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory.into());
        }
        let source_host = crate::guest_host_share_path(&source_absolute).map(|p| p.to_owned());
        let destination_host =
            crate::guest_host_share_path(&destination_absolute).map(|p| p.to_owned());
        if source_host.is_some() || destination_host.is_some() {
            if !destination_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
                return Err(fs_types::ErrorCode::ReadOnly.into());
            }
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
                .hard_link(&source_host, &destination_host)
                .await
                .map_err(map_host_fs_error)?;
            accessor.with(|mut access| {
                access
                    .get()
                    .filesystem_mut()
                    .invalidate_host_subtree(&destination_absolute);
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
                .link_at(
                    &source_base,
                    &source_path,
                    &destination_base,
                    &destination_path,
                    now_nanos,
                )
                .map_err(Into::into)
        })
    }

    async fn open_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
        open_flags: fs_types::OpenFlags,
        flags: fs_types::DescriptorFlags,
    ) -> Result<Resource<FsDescriptor>, FsError> {
        let profile = accessor.with(|mut access| component_fs_profile(access.get()));
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
                    let kind = host_metadata_node_kind(&metadata);
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
                    let descriptor_flags =
                        effective_open_descriptor_flags(base_flags, flags, kind)?;
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
                    // Host files are not materialised in kernel memory here:
                    // the descriptor only records where the file lives, and
                    // `read-via-stream` pulls bounded chunks over 9p as the
                    // guest consumes them.
                    if kind == FsNodeKind::File {
                        let opened = FsDescriptor {
                            path: absolute,
                            kind,
                            flags: descriptor_flags,
                            identity: Some(metadata.identity),
                        };
                        let result = accessor
                            .with(|mut access| access.get().table.push(opened))
                            .map_err(FsError::trap);
                        record_component_fs_profile(profile, "open-at");
                        return result;
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
                        flags: descriptor_flags,
                        identity: Some(metadata.identity),
                    };
                    let result = accessor.with(|mut access| {
                        access
                            .get()
                            .filesystem_mut()
                            .seed_host_directory_entries(&absolute, entries);
                        access.get().table.push(opened).map_err(FsError::trap)
                    });
                    record_component_fs_profile(profile, "open-at");
                    return result;
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
                        let metadata = service
                            .stat_path(&host_path)
                            .await
                            .map_err(map_host_fs_error)?;
                        let descriptor_flags =
                            effective_open_descriptor_flags(base_flags, flags, FsNodeKind::File)?;
                        let opened = FsDescriptor {
                            path: absolute,
                            kind: FsNodeKind::File,
                            flags: descriptor_flags,
                            identity: Some(metadata.identity),
                        };
                        let result = accessor.with(|mut access| {
                            access.get().table.push(opened).map_err(FsError::trap)
                        });
                        record_component_fs_profile(profile, "open-at");
                        return result;
                    }
                    return Err(err.into());
                }
            }
        }

        // Embedded filesystem path — fully synchronous.
        let result = accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            let now_nanos = access.get().now_nanos();
            let opened = access
                .get()
                .filesystem_mut()
                .open_at(&base, path_flags, &path, open_flags, flags, now_nanos)
                .map_err(FsError::from)?;
            let resource = access.get().table.push(opened).map_err(FsError::trap)?;
            Ok(resource)
        });
        record_component_fs_profile(profile, "open-at");
        result
    }

    async fn readlink_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<String, FsError> {
        let allowed = accessor
            .with(|mut access| {
                Ok::<_, wasmtime::Error>(
                    access
                        .get()
                        .process_authority()
                        .derive_symlink_read_cap()
                        .is_ok(),
                )
            })
            .map_err(FsError::trap)?;
        if !allowed {
            return Err(fs_types::ErrorCode::NotPermitted.into());
        }
        let (absolute, host_path) = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            if descriptor.kind != FsNodeKind::Directory {
                return Err(fs_types::ErrorCode::NotDirectory.into());
            }
            let absolute = crate::resolve_child_path(&descriptor.path, &path)
                .map_err(map_component_fs_path_error)?;
            let host_path = crate::guest_host_share_path(&absolute).map(|p| p.to_owned());
            Ok::<_, FsError>((absolute, host_path))
        })?;
        if let Some(host_path) = host_path {
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            let payload = service
                .read_link(&host_path)
                .await
                .map_err(map_host_fs_error)?;
            resolve_symlink_payload(&absolute, &payload)?;
            return Ok(payload);
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            access
                .get()
                .filesystem()
                .readlink_at(&descriptor, &path)
                .map_err(Into::into)
        })
    }

    async fn remove_directory_at(
        accessor: &Accessor<U, Self>,
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

    async fn rename_at(
        accessor: &Accessor<U, Self>,
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

    async fn symlink_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        old_path: String,
        new_path: String,
    ) -> Result<(), FsError> {
        let allowed = accessor
            .with(|mut access| {
                Ok::<_, wasmtime::Error>(
                    access
                        .get()
                        .process_authority()
                        .derive_symlink_create_cap()
                        .is_ok(),
                )
            })
            .map_err(FsError::trap)?;
        if !allowed {
            return Err(fs_types::ErrorCode::NotPermitted.into());
        }
        let (absolute, base_flags) = accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            if descriptor.kind != FsNodeKind::Directory {
                return Err(fs_types::ErrorCode::NotDirectory.into());
            }
            let absolute = crate::resolve_child_path(&descriptor.path, &new_path)
                .map_err(map_component_fs_path_error)?;
            resolve_symlink_payload(&absolute, &old_path)?;
            Ok::<_, FsError>((absolute, descriptor.flags))
        })?;
        if let Some(host_path) = crate::guest_host_share_path(&absolute).map(|p| p.to_owned()) {
            if !base_flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
                return Err(fs_types::ErrorCode::ReadOnly.into());
            }
            let service = accessor.with(|mut access| {
                access
                    .get()
                    .filesystem()
                    .host_service()
                    .map_err(FsError::from)
            })?;
            service
                .symlink(&old_path, &host_path)
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
                .symlink_at(&descriptor, &new_path, &old_path, now_nanos)
                .map_err(Into::into)
        })
    }

    async fn unlink_file_at(
        accessor: &Accessor<U, Self>,
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

    async fn is_same_object(
        accessor: &Accessor<U, Self>,
        a: Resource<FsDescriptor>,
        b: Resource<FsDescriptor>,
    ) -> Result<bool> {
        accessor.with(|mut access| {
            let left = access.get().table.get(&a)?.clone();
            let right = access.get().table.get(&b)?.clone();
            Ok(left
                .identity
                .zip(right.identity)
                .is_some_and(|(left, right)| left == right))
        })
    }

    async fn metadata_hash(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::MetadataHashValue, FsError> {
        let profile = accessor.with(|mut access| component_fs_profile(access.get()));
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
            let result = metadata_hash_value(
                metadata.identity,
                u64::from(metadata.mode) << 32 ^ metadata.size,
            );
            record_component_fs_profile(profile, "metadata-hash");
            return Ok(result);
        }
        let result = accessor.with(|mut access| {
            access
                .get()
                .filesystem_mut()
                .metadata_hash(&path)
                .map_err(Into::into)
        });
        record_component_fs_profile(profile, "metadata-hash");
        result
    }

    async fn metadata_hash_at(
        accessor: &Accessor<U, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::MetadataHashValue, FsError> {
        let profile = accessor.with(|mut access| component_fs_profile(access.get()));
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
            let result = metadata_hash_value(
                metadata.identity,
                u64::from(metadata.mode) << 32 ^ metadata.size,
            );
            record_component_fs_profile(profile, "metadata-hash-at");
            return Ok(result);
        }
        let result = accessor.with(|mut access| {
            access
                .get()
                .filesystem_mut()
                .metadata_hash(&absolute)
                .map_err(Into::into)
        });
        record_component_fs_profile(profile, "metadata-hash-at");
        result
    }
}

/// Flushes a descriptor's host-side buffers.
///
/// `sync` and `sync-data` differ only in whether inode metadata is included;
/// 9p exposes a single `Tfsync`, which the host translates to `fsync`, so both
/// take the same path for the host share. The embedded filesystem holds every
/// node in memory with no write-back stage, so there is nothing a sync could
/// flush there — that no-op is the correct answer, not a missing feature, and
/// is spelled out here rather than left implicit.
async fn sync_descriptor<U, CpuImpl, HostFs>(
    accessor: &Accessor<U, HasSelf<StoreData<CpuImpl, HostFs>>>,
    descriptor: Resource<FsDescriptor>,
) -> Result<(), FsError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let path = accessor.with(|mut access| {
        let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
        Ok::<_, FsError>(descriptor.path.clone())
    })?;
    let Some(host_path) = crate::guest_host_share_path(&path).map(|path| path.to_owned()) else {
        return Ok(());
    };
    let service = accessor.with(|mut access| {
        access
            .get()
            .filesystem()
            .host_service()
            .map_err(FsError::from)
    })?;
    service
        .sync_file(&host_path)
        .await
        .map_err(map_host_fs_error)?;
    Ok(())
}

pub(super) fn get_fs_descriptor<CpuImpl, HostFs>(
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

pub(super) fn is_dir_first(kind: fs_types::DescriptorType) -> u8 {
    match kind {
        fs_types::DescriptorType::Directory => 0,
        _ => 1,
    }
}

pub(super) fn map_component_fs_path_error(
    error: crate::ComponentFsPathError,
) -> fs_types::ErrorCode {
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
