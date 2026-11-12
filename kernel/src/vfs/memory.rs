//! A writable filesystem held entirely in kernel memory.
//!
//! The tree is an inode table and a name index ordered by path, so the
//! names below one directory form one contiguous range and a hard link is
//! a second name for the same inode rather than a copy of it.
//!
//! Concurrency contract: one spin mutex guards a tree. Every operation
//! takes it for its own body, which never awaits and never allocates while
//! another processor could be waiting for more than the copy of the bytes
//! it writes, and no lock is held across an `.await`. No interrupt handler
//! reaches a tree, so the lock does not mask interrupts.
//!
//! A tree may be the upper layer of an overlay ([`super::RootFs`]). It
//! then also records what the overlay hid of the layer below: whiteouts,
//! names removed from that layer, and opaque directories, which show
//! nothing of it. A tree nothing overlays holds neither, and every
//! operation on it behaves as if they did not exist. Both live under the
//! tree's one lock, so an overlay changes an object and what it hides in
//! one step.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::future::Future;
use core::ops::Bound;

use bytes::{Bytes, BytesMut};
use hashbrown::{HashMap, HashSet};
use helios_hal::fs::{
    DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Metadata, NodeId,
    NodeKind, OpenFlags, WritePosition,
};
use spin::Mutex;
use triomphe::Arc;

use super::cmp_to_children;

const ROOT_INODE: u64 = 1;

/// A tree in kernel memory. Clones share the tree.
#[derive(Clone)]
pub struct MemoryFs {
    device: DeviceId,
    state: Arc<Mutex<MemoryState>>,
}

impl core::fmt::Debug for MemoryFs {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MemoryFs")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

struct Inode {
    kind: NodeKind,
    /// A file's bytes or a symbolic link's target text.
    contents: Bytes,
    link_count: u64,
    access_nanos: u64,
    modified_nanos: u64,
    status_nanos: u64,
}

struct MemoryState {
    /// Every name but the root's, by path from the root.
    names: BTreeMap<String, u64>,
    inodes: HashMap<u64, Inode>,
    next_inode: u64,
    /// Names an overlay removed from the layer below. Each lies inside a
    /// directory of this tree and names nothing in it.
    whiteouts: BTreeSet<String>,
    /// Directories an overlay shows without the layer below merged in.
    opaque: HashSet<u64>,
}

/// What a tree that is an overlay's upper layer holds at one path.
pub(super) struct UpperView {
    /// The tree's own object there.
    pub(super) object: Option<Metadata>,
    /// Whether that object is a directory showing nothing of the layer
    /// below.
    pub(super) opaque: bool,
    /// Whether the layer below shows at this path once the tree's own
    /// object is out of the way: no whiteout at it or above it, and every
    /// directory of the tree above it is a merging one.
    pub(super) beneath: bool,
}

impl MemoryFs {
    /// An empty tree with only its root directory.
    pub fn new(device: DeviceId) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_INODE,
            Inode {
                kind: NodeKind::Directory,
                contents: Bytes::new(),
                link_count: 1,
                access_nanos: 0,
                modified_nanos: 0,
                status_nanos: 0,
            },
        );
        Self {
            device,
            state: Arc::new(Mutex::new(MemoryState {
                names: BTreeMap::new(),
                inodes,
                next_inode: ROOT_INODE + 1,
                whiteouts: BTreeSet::new(),
                opaque: HashSet::new(),
            })),
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// Whether `other` is the same tree, not merely an equal one.
    pub fn same_tree(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// What this tree, as an overlay's upper layer, holds at `path`.
    pub(super) fn view(&self, path: FsPath<'_>) -> FsResult<UpperView> {
        let state = self.state.lock();
        let object = match state.metadata(self.device, path) {
            Ok(metadata) => Some(metadata),
            Err(FsError::NoEntry) => None,
            Err(error) => return Err(error),
        };
        let opaque = object.is_some()
            && state
                .inode_of(path)
                .is_ok_and(|number| state.opaque.contains(&number));
        Ok(UpperView {
            object,
            opaque,
            beneath: state.beneath(path),
        })
    }

    /// Copies an object of the layer below up to `path`, keeping its
    /// times: `contents` is a file's bytes or a link's target, and empty
    /// for a directory, which stays merging. When the tree has an object
    /// there already, another copy-up won and this one does nothing; when
    /// the layer below no longer shows there, the object was removed
    /// meanwhile: [`FsError::NoEntry`].
    pub(super) fn copy_up(
        &self,
        path: FsPath<'_>,
        kind: NodeKind,
        contents: Bytes,
        original: &Metadata,
    ) -> FsResult<()> {
        let mut state = self.state.lock();
        if state.inode_of(path).is_ok() {
            return Ok(());
        }
        if !state.beneath(path) {
            return Err(FsError::NoEntry);
        }
        state.require_new_name(path)?;
        let number = state.insert(path, kind, contents, original.modified_nanos);
        let inode = state
            .inodes
            .get_mut(&number)
            .expect("the inode was just inserted");
        inode.access_nanos = original.access_nanos;
        inode.status_nanos = original.status_nanos;
        Ok(())
    }

    /// Removes the object at `path` (a directory when `directory`, which
    /// must hold no names) and, when `whiteout`, records a whiteout there
    /// so the layer below stays hidden. With `whiteout`, a path the tree
    /// has no object at is the layer below's alone, and only the whiteout
    /// is recorded.
    pub(super) fn remove(&self, path: FsPath<'_>, directory: bool, whiteout: bool) -> FsResult<()> {
        if path.is_root() {
            return Err(if directory {
                FsError::NotPermitted
            } else {
                FsError::IsDirectory
            });
        }
        let mut state = self.state.lock();
        match state.inode(path) {
            Ok((_, inode)) => {
                match (directory, inode.kind == NodeKind::Directory) {
                    (true, false) => return Err(FsError::NotDirectory),
                    (false, true) => return Err(FsError::IsDirectory),
                    (true, true) if state.children(path).next().is_some() => {
                        return Err(FsError::NotEmpty);
                    }
                    _ => {}
                }
                state.remove_name(path);
            }
            Err(FsError::NoEntry) if whiteout && state.beneath(path) => {}
            Err(error) => return Err(error),
        }
        if directory {
            state.drop_whiteouts_below(path);
        }
        if whiteout {
            state.whiteouts.insert(path.as_str().to_string());
        }
        Ok(())
    }

    /// Renames as [`FileSystem::rename`] does, then records a whiteout at
    /// `source` when `whiteout`, so the layer below stays hidden there.
    pub(super) fn rename_over(
        &self,
        source: FsPath<'_>,
        destination: FsPath<'_>,
        now: u64,
        whiteout: bool,
    ) -> FsResult<()> {
        if source.is_root() || destination.is_root() {
            return Err(FsError::NotPermitted);
        }
        let mut state = self.state.lock();
        let (_, inode) = state.inode(source)?;
        if source == destination {
            return Ok(());
        }
        if inode.kind == NodeKind::Directory && destination.starts_with(source) {
            return Err(FsError::NotPermitted);
        }
        state.require_new_name(destination)?;
        let source_inode = state.inode_of(source)?;
        let moved: Vec<(String, u64)> =
            core::iter::once((source.as_str().to_string(), source_inode))
                .chain(
                    state
                        .descendants(source)
                        .map(|(name, number)| (name.clone(), *number)),
                )
                .collect();
        let renamed = |name: &str| {
            let mut renamed = String::with_capacity(
                destination.as_str().len() + name.len() - source.as_str().len(),
            );
            renamed.push_str(destination.as_str());
            renamed.push_str(&name[source.as_str().len()..]);
            renamed
        };
        for (name, number) in moved {
            state.names.remove(&name);
            state.names.insert(renamed(&name), number);
            state
                .inodes
                .get_mut(&number)
                .expect("every name refers to a live inode")
                .status_nanos = now;
        }
        for name in state.whiteouts_below(source) {
            state.whiteouts.remove(&name);
            state.whiteouts.insert(renamed(&name));
        }
        state.take_whiteout(destination, source_inode);
        if whiteout {
            state.whiteouts.insert(source.as_str().to_string());
        }
        Ok(())
    }
}

impl MemoryState {
    fn inode_of(&self, path: FsPath<'_>) -> FsResult<u64> {
        if path.is_root() {
            return Ok(ROOT_INODE);
        }
        self.names
            .get(path.as_str())
            .copied()
            .ok_or(FsError::NoEntry)
    }

    fn inode(&self, path: FsPath<'_>) -> FsResult<(u64, &Inode)> {
        let number = self.inode_of(path)?;
        Ok((number, &self.inodes[&number]))
    }

    fn inode_mut(&mut self, path: FsPath<'_>) -> FsResult<&mut Inode> {
        let number = self.inode_of(path)?;
        Ok(self
            .inodes
            .get_mut(&number)
            .expect("every name refers to a live inode"))
    }

    fn metadata(&self, device: DeviceId, path: FsPath<'_>) -> FsResult<Metadata> {
        let (number, inode) = self.inode(path)?;
        Ok(Metadata {
            id: NodeId::new(device, number),
            kind: inode.kind,
            link_count: inode.link_count,
            size: match inode.kind {
                NodeKind::Directory => 0,
                NodeKind::File | NodeKind::Symlink => inode.contents.len() as u64,
            },
            access_nanos: inode.access_nanos,
            modified_nanos: inode.modified_nanos,
            status_nanos: inode.status_nanos,
        })
    }

    /// Checks that `path` is absent and its parent is a directory.
    fn require_new_name(&self, path: FsPath<'_>) -> FsResult<()> {
        let Some((parent, _)) = path.split_last() else {
            return Err(FsError::Exists);
        };
        if self.inode_of(path).is_ok() {
            return Err(FsError::Exists);
        }
        match self.inode(parent)?.1.kind {
            NodeKind::Directory => Ok(()),
            NodeKind::File | NodeKind::Symlink => Err(FsError::NotDirectory),
        }
    }

    fn insert(&mut self, path: FsPath<'_>, kind: NodeKind, contents: Bytes, now: u64) -> u64 {
        let number = self.next_inode;
        self.next_inode += 1;
        self.inodes.insert(
            number,
            Inode {
                kind,
                contents,
                link_count: 1,
                access_nanos: now,
                modified_nanos: now,
                status_nanos: now,
            },
        );
        self.names.insert(path.as_str().to_string(), number);
        self.take_whiteout(path, number);
        number
    }

    /// A new name at `path` for inode `number` replaces any whiteout
    /// there. A directory that does is opaque: what the layer below had
    /// at that path stays removed, its contents included.
    fn take_whiteout(&mut self, path: FsPath<'_>, number: u64) {
        if self.whiteouts.remove(path.as_str()) && self.inodes[&number].kind == NodeKind::Directory
        {
            self.opaque.insert(number);
        }
    }

    /// Whether the layer below an overlay shows at `path` once this tree's
    /// own object there is out of the way: no whiteout at `path` or above
    /// it, and every name of this tree above it a merging directory.
    fn beneath(&self, path: FsPath<'_>) -> bool {
        let path = path.as_str();
        if self.whiteouts.is_empty() && self.opaque.is_empty() {
            return true;
        }
        let ancestors = path.match_indices('/').map(|(end, _)| &path[..end]);
        for ancestor in ancestors {
            if self.whiteouts.contains(ancestor) {
                return false;
            }
            if let Some(number) = self.names.get(ancestor)
                && (self.inodes[number].kind != NodeKind::Directory || self.opaque.contains(number))
            {
                return false;
            }
        }
        !self.whiteouts.contains(path)
    }

    /// The whiteouts below the directory at `path`.
    fn whiteouts_below(&self, path: FsPath<'_>) -> Vec<String> {
        let directory = path.as_str();
        self.whiteouts
            .range::<str, _>((Bound::Excluded(directory), Bound::Unbounded))
            .skip_while(|name| cmp_to_children(name, directory) == Ordering::Less)
            .take_while(|name| cmp_to_children(name, directory) == Ordering::Equal)
            .cloned()
            .collect()
    }

    /// Forgets the whiteouts below a directory that is gone.
    fn drop_whiteouts_below(&mut self, path: FsPath<'_>) {
        for name in self.whiteouts_below(path) {
            self.whiteouts.remove(&name);
        }
    }

    /// Drops the name at `path`, and the inode with its last name.
    fn remove_name(&mut self, path: FsPath<'_>) {
        let number = self
            .names
            .remove(path.as_str())
            .expect("a removed name was looked up first");
        let inode = self
            .inodes
            .get_mut(&number)
            .expect("every name refers to a live inode");
        inode.link_count -= 1;
        if inode.link_count == 0 {
            self.inodes.remove(&number);
            self.opaque.remove(&number);
        }
    }

    /// Every name below the directory at `path`, in order.
    fn descendants<'a>(&'a self, path: FsPath<'a>) -> impl Iterator<Item = (&'a String, &'a u64)> {
        let directory = path.as_str();
        self.names
            .range::<str, _>((Bound::Excluded(directory), Bound::Unbounded))
            .skip_while(move |(name, _)| {
                !path.is_root() && cmp_to_children(name, directory) == Ordering::Less
            })
            .take_while(move |(name, _)| {
                path.is_root() || cmp_to_children(name, directory) == Ordering::Equal
            })
    }

    /// The names directly below the directory at `path`, in order.
    fn children<'a>(&'a self, path: FsPath<'a>) -> impl Iterator<Item = (&'a str, u64)> + 'a {
        let prefix_len = if path.is_root() {
            0
        } else {
            path.as_str().len() + 1
        };
        self.descendants(path).filter_map(move |(name, number)| {
            let child = &name[prefix_len..];
            (!child.contains('/')).then_some((child, *number))
        })
    }

    /// Replaces a file's bytes with what `edit` makes of them.
    fn edit_contents(
        &mut self,
        path: FsPath<'_>,
        now: u64,
        edit: impl FnOnce(&mut BytesMut) -> FsResult<()>,
    ) -> FsResult<()> {
        let inode = self.inode_mut(path)?;
        match inode.kind {
            NodeKind::File => {}
            NodeKind::Directory => return Err(FsError::IsDirectory),
            NodeKind::Symlink => return Err(FsError::Invalid),
        }
        let contents = core::mem::take(&mut inode.contents);
        let mut contents = contents
            .try_into_mut()
            .unwrap_or_else(|shared| BytesMut::from(shared.as_ref()));
        let edited = edit(&mut contents);
        inode.contents = contents.freeze();
        edited?;
        inode.modified_nanos = now;
        inode.status_nanos = now;
        Ok(())
    }
}

impl MemoryFs {
    /// [`FileSystem::write_with`], done at once: `fill` writes straight
    /// into the file's storage.
    pub(super) fn write_now(
        &self,
        path: FsPath<'_>,
        position: WritePosition,
        len: usize,
        now: u64,
        fill: impl FnOnce(&mut [u8]),
    ) -> FsResult<u64> {
        let mut landed = 0;
        self.state.lock().edit_contents(path, now, |contents| {
            let at = match position {
                WritePosition::At(offset) => {
                    usize::try_from(offset).map_err(|_| FsError::Overflow)?
                }
                WritePosition::End => contents.len(),
            };
            let end = at.checked_add(len).ok_or(FsError::Overflow)?;
            if contents.len() < end {
                contents.resize(end, 0);
            }
            fill(&mut contents[at..end]);
            landed = at as u64;
            Ok(())
        })?;
        Ok(landed)
    }
}

impl FileSystem for MemoryFs {
    async fn stat(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        self.state.lock().metadata(self.device, path)
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, now: u64) -> FsResult<Metadata> {
        let mut state = self.state.lock();
        match state.inode(path) {
            Ok((_, inode)) => {
                if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUSIVE) {
                    return Err(FsError::Exists);
                }
                if flags.contains(OpenFlags::DIRECTORY) && inode.kind != NodeKind::Directory {
                    return Err(FsError::NotDirectory);
                }
                if flags.contains(OpenFlags::TRUNCATE) {
                    state.edit_contents(path, now, |contents| {
                        contents.clear();
                        Ok(())
                    })?;
                }
            }
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                if flags.contains(OpenFlags::DIRECTORY) {
                    return Err(FsError::Invalid);
                }
                state.require_new_name(path)?;
                state.insert(path, NodeKind::File, Bytes::new(), now);
            }
            Err(error) => return Err(error),
        }
        state.metadata(self.device, path)
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        let state = self.state.lock();
        let (_, inode) = state.inode(path)?;
        match inode.kind {
            NodeKind::File => {}
            NodeKind::Directory => return Err(FsError::IsDirectory),
            NodeKind::Symlink => return Err(FsError::Invalid),
        }
        let offset = usize::try_from(offset).map_err(|_| FsError::Overflow)?;
        if offset >= inode.contents.len() {
            return Ok(Bytes::new());
        }
        let end = offset.saturating_add(max_bytes).min(inode.contents.len());
        Ok(inode.contents.slice(offset..end))
    }

    fn write_with<'a, F>(
        &'a self,
        path: FsPath<'a>,
        position: WritePosition,
        len: usize,
        now: u64,
        fill: F,
    ) -> impl Future<Output = FsResult<u64>> + Send + 'a
    where
        F: FnOnce(&mut [u8]),
    {
        // The write is done here, under the lock; the future only reports
        // it, so it holds nothing of `fill`.
        core::future::ready(self.write_now(path, position, len, now, fill))
    }

    async fn set_size(&self, path: FsPath<'_>, size: u64, now: u64) -> FsResult<()> {
        let size = usize::try_from(size).map_err(|_| FsError::Overflow)?;
        self.state.lock().edit_contents(path, now, |contents| {
            contents.resize(size, 0);
            Ok(())
        })
    }

    async fn set_times(
        &self,
        path: FsPath<'_>,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        now: u64,
    ) -> FsResult<()> {
        let mut state = self.state.lock();
        let inode = state.inode_mut(path)?;
        if let Some(access_nanos) = access_nanos {
            inode.access_nanos = access_nanos;
        }
        if let Some(modified_nanos) = modified_nanos {
            inode.modified_nanos = modified_nanos;
        }
        if access_nanos.is_some() || modified_nanos.is_some() {
            inode.status_nanos = now;
        }
        Ok(())
    }

    async fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        let state = self.state.lock();
        if state.inode(path)?.1.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let skip = usize::try_from(cookie.raw()).map_err(|_| FsError::Invalid)?;
        Ok(state
            .children(path)
            .enumerate()
            .skip(skip)
            .take(max_entries)
            .map(|(position, (name, number))| DirEntry {
                name: String::from(name),
                kind: state.inodes[&number].kind,
                inode: number,
                next: DirCookie::new(position as u64 + 1),
            })
            .collect())
    }

    async fn create_directory(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        let mut state = self.state.lock();
        state.require_new_name(path)?;
        state.insert(path, NodeKind::Directory, Bytes::new(), now);
        Ok(())
    }

    async fn remove_directory(&self, path: FsPath<'_>) -> FsResult<()> {
        self.remove(path, true, false)
    }

    async fn unlink(&self, path: FsPath<'_>) -> FsResult<()> {
        self.remove(path, false, false)
    }

    async fn rename(&self, source: FsPath<'_>, destination: FsPath<'_>, now: u64) -> FsResult<()> {
        self.rename_over(source, destination, now, false)
    }

    async fn link(&self, existing: FsPath<'_>, link: FsPath<'_>, now: u64) -> FsResult<()> {
        let mut state = self.state.lock();
        let (number, inode) = state.inode(existing)?;
        if inode.kind == NodeKind::Directory {
            return Err(FsError::NotPermitted);
        }
        state.require_new_name(link)?;
        state.names.insert(link.as_str().to_string(), number);
        state.take_whiteout(link, number);
        let inode = state
            .inodes
            .get_mut(&number)
            .expect("every name refers to a live inode");
        inode.link_count = inode.link_count.checked_add(1).ok_or(FsError::Overflow)?;
        inode.status_nanos = now;
        Ok(())
    }

    async fn symlink(&self, target: &str, link: FsPath<'_>, now: u64) -> FsResult<()> {
        let mut state = self.state.lock();
        state.require_new_name(link)?;
        state.insert(
            link,
            NodeKind::Symlink,
            Bytes::copy_from_slice(target.as_bytes()),
            now,
        );
        Ok(())
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        let state = self.state.lock();
        let (_, inode) = state.inode(path)?;
        if inode.kind != NodeKind::Symlink {
            return Err(FsError::Invalid);
        }
        core::str::from_utf8(&inode.contents)
            .map(String::from)
            .map_err(|_| FsError::IllegalByteSequence)
    }

    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        // Every byte lives here and nowhere else: there is no later stage a
        // sync could push it to.
        self.state.lock().inode_of(path).map(drop)
    }

    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        self.state.lock().inode_of(path).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec::Vec;

    use futures_lite::future::block_on;
    use helios_hal::fs::{
        DeviceId, DirCookie, FileSystem, FsError, FsPath, NodeKind, OpenFlags, WritePosition,
    };

    use super::MemoryFs;
    use crate::vfs::write_bytes;

    fn path(path: &str) -> FsPath<'_> {
        FsPath::new(path).expect("test paths are valid")
    }

    fn tree() -> MemoryFs {
        MemoryFs::new(DeviceId::new(9))
    }

    fn names(fs: &MemoryFs, directory: &str) -> Vec<String> {
        block_on(fs.read_dir(path(directory), DirCookie::START, usize::MAX))
            .expect("directory lists")
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    /// Creates of one path that race on several processors: one wins,
    /// every other is told the name exists, and none panics. Each round
    /// races the four ways a name comes into being.
    #[test]
    fn racing_creates_of_one_path_leave_one_winner() {
        use std::sync::Barrier;
        use std::thread;

        const RACERS: usize = 8;
        const ROUNDS: usize = 200;

        let fs = tree();
        block_on(fs.open(path("existing"), OpenFlags::CREATE, 0)).expect("creates");
        for round in 0..ROUNDS {
            let name = alloc::format!("raced-{round}");
            let barrier = Barrier::new(RACERS);
            let outcomes: Vec<Result<(), FsError>> = thread::scope(|scope| {
                let racers: Vec<_> = (0..RACERS)
                    .map(|racer| {
                        let (fs, name, barrier) = (&fs, &name, &barrier);
                        scope.spawn(move || {
                            let new = path(name);
                            barrier.wait();
                            block_on(async {
                                match racer % 4 {
                                    0 => fs
                                        .open(new, OpenFlags::CREATE | OpenFlags::EXCLUSIVE, 1)
                                        .await
                                        .map(drop),
                                    1 => fs.create_directory(new, 1).await,
                                    2 => fs.symlink("existing", new, 1).await,
                                    _ => fs.link(path("existing"), new, 1).await,
                                }
                            })
                        })
                    })
                    .collect();
                racers
                    .into_iter()
                    .map(|racer| racer.join().expect("no create panics"))
                    .collect()
            });
            let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
            assert_eq!(winners, 1, "round {round}: {outcomes:?}");
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| matches!(outcome, Ok(()) | Err(FsError::Exists))),
                "round {round}: {outcomes:?}"
            );
            block_on(fs.stat(path(&name))).expect("the winner's node is there");
        }
    }

    #[test]
    fn creates_writes_and_reads_back_positionally() {
        let fs = tree();
        block_on(fs.open(path("f"), OpenFlags::CREATE, 5)).expect("creates");
        assert_eq!(
            block_on(write_bytes(&fs, path("f"), WritePosition::At(2), b"cd", 6)),
            Ok(2),
            "writes past the end"
        );
        // The filesystem hands its own storage to the filler.
        assert_eq!(
            block_on(
                fs.write_with(path("f"), WritePosition::End, 2, 7, |destination| {
                    assert_eq!(destination, [0, 0]);
                    destination.copy_from_slice(b"ef");
                })
            ),
            Ok(4)
        );
        let read = block_on(fs.read_at(path("f"), 0, 64)).expect("reads");
        assert_eq!(read.as_ref(), b"\0\0cdef");
        let stat = block_on(fs.stat(path("f"))).expect("stats");
        assert_eq!((stat.size, stat.modified_nanos), (6, 7));
        block_on(fs.open(path("f"), OpenFlags::TRUNCATE, 8)).expect("truncates");
        assert_eq!(block_on(fs.stat(path("f"))).expect("stats").size, 0);
    }

    #[test]
    fn a_hard_link_shares_the_inode_until_the_last_name_goes() {
        let fs = tree();
        block_on(fs.open(path("a"), OpenFlags::CREATE, 0)).expect("creates");
        block_on(fs.link(path("a"), path("b"), 1)).expect("links");
        block_on(write_bytes(
            &fs,
            path("b"),
            WritePosition::At(0),
            b"shared",
            2,
        ))
        .expect("writes");
        assert_eq!(
            block_on(fs.read_at(path("a"), 0, 64))
                .expect("reads")
                .as_ref(),
            b"shared"
        );
        let a = block_on(fs.stat(path("a"))).expect("stats");
        assert_eq!(
            (a.link_count, a.id),
            (2, block_on(fs.stat(path("b"))).expect("stats").id)
        );
        block_on(fs.unlink(path("a"))).expect("unlinks");
        assert_eq!(block_on(fs.stat(path("b"))).expect("stats").link_count, 1);
    }

    #[test]
    fn rename_moves_a_subtree_and_refuses_moving_into_itself() {
        let fs = tree();
        block_on(fs.create_directory(path("d"), 0)).expect("creates");
        block_on(fs.open(path("d/f"), OpenFlags::CREATE, 0)).expect("creates");
        block_on(fs.create_directory(path("dx"), 0)).expect("creates a prefix sibling");
        // Sorts between `d` and `d/f`.
        block_on(fs.open(path("d.x"), OpenFlags::CREATE, 0)).expect("creates");
        assert_eq!(
            block_on(fs.rename(path("d"), path("d/inner"), 1)),
            Err(FsError::NotPermitted)
        );
        block_on(fs.rename(path("d"), path("e"), 1)).expect("renames");
        assert_eq!(names(&fs, ""), ["d.x", "dx", "e"]);
        assert_eq!(names(&fs, "e"), ["f"]);
        assert_eq!(
            block_on(fs.remove_directory(path("e"))),
            Err(FsError::NotEmpty)
        );
    }

    #[test]
    fn listing_skips_grandchildren_and_resumes() {
        let fs = tree();
        for directory in ["a", "a/b", "c"] {
            block_on(fs.create_directory(path(directory), 0)).expect("creates");
        }
        block_on(fs.symlink("a", path("l"), 0)).expect("links");
        assert_eq!(names(&fs, ""), ["a", "c", "l"]);
        let first = block_on(fs.read_dir(FsPath::ROOT, DirCookie::START, 1)).expect("lists");
        let rest = block_on(fs.read_dir(FsPath::ROOT, first[0].next, 8)).expect("lists");
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[1].kind, NodeKind::Symlink);
        assert_eq!(block_on(fs.read_link(path("l"))).as_deref(), Ok("a"));
    }
}
