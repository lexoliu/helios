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

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ops::Bound;

use bytes::{Bytes, BytesMut};
use hashbrown::HashMap;
use helios_hal::fs::{
    DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Metadata, NodeId,
    NodeKind, OpenFlags,
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

    fn insert(&mut self, path: FsPath<'_>, kind: NodeKind, contents: Bytes, now: u64) {
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

fn write_into(contents: &mut BytesMut, offset: u64, data: &[u8]) -> FsResult<()> {
    let offset = usize::try_from(offset).map_err(|_| FsError::Overflow)?;
    let end = offset.checked_add(data.len()).ok_or(FsError::Overflow)?;
    if contents.len() < end {
        contents.resize(end, 0);
    }
    contents[offset..end].copy_from_slice(data);
    Ok(())
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

    async fn write_at(&self, path: FsPath<'_>, offset: u64, data: &[u8], now: u64) -> FsResult<()> {
        self.state
            .lock()
            .edit_contents(path, now, |contents| write_into(contents, offset, data))
    }

    async fn append(&self, path: FsPath<'_>, data: &[u8], now: u64) -> FsResult<u64> {
        let mut at = 0;
        self.state.lock().edit_contents(path, now, |contents| {
            at = contents.len() as u64;
            contents.extend_from_slice(data);
            Ok(())
        })?;
        Ok(at)
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
        if path.is_root() {
            return Err(FsError::NotPermitted);
        }
        let mut state = self.state.lock();
        if state.inode(path)?.1.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if state.children(path).next().is_some() {
            return Err(FsError::NotEmpty);
        }
        state.remove_name(path);
        Ok(())
    }

    async fn unlink(&self, path: FsPath<'_>) -> FsResult<()> {
        let mut state = self.state.lock();
        if path.is_root() || state.inode(path)?.1.kind == NodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        state.remove_name(path);
        Ok(())
    }

    async fn rename(&self, source: FsPath<'_>, destination: FsPath<'_>, now: u64) -> FsResult<()> {
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
        for (name, number) in moved {
            state.names.remove(&name);
            let mut renamed = String::with_capacity(
                destination.as_str().len() + name.len() - source.as_str().len(),
            );
            renamed.push_str(destination.as_str());
            renamed.push_str(&name[source.as_str().len()..]);
            state.names.insert(renamed, number);
            state
                .inodes
                .get_mut(&number)
                .expect("every name refers to a live inode")
                .status_nanos = now;
        }
        Ok(())
    }

    async fn link(&self, existing: FsPath<'_>, link: FsPath<'_>, now: u64) -> FsResult<()> {
        let mut state = self.state.lock();
        let (number, inode) = state.inode(existing)?;
        if inode.kind == NodeKind::Directory {
            return Err(FsError::NotPermitted);
        }
        state.require_new_name(link)?;
        state.names.insert(link.as_str().to_string(), number);
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
    use helios_hal::fs::{DeviceId, DirCookie, FileSystem, FsError, FsPath, NodeKind, OpenFlags};

    use super::MemoryFs;

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

    #[test]
    fn creates_writes_and_reads_back_positionally() {
        let fs = tree();
        block_on(fs.open(path("f"), OpenFlags::CREATE, 5)).expect("creates");
        block_on(fs.write_at(path("f"), 2, b"cd", 6)).expect("writes past the end");
        assert_eq!(block_on(fs.append(path("f"), b"ef", 7)), Ok(4));
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
        block_on(fs.write_at(path("b"), 0, b"shared", 2)).expect("writes");
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
