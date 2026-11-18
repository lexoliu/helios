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
use helios_btrfs::DeviceNumber;
use helios_btrfs::write::Special;
use helios_hal::fs::{
    DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Landed, Metadata, NodeId,
    NodeKind, OpenFlags, OwnedFsPath, PreparedWrite, RenameMode, WritePosition, WriteProgress,
};
use spin::Mutex;
use triomphe::Arc;

use super::{UnixAttributes, cmp_to_children};

const ROOT_INODE: u64 = 1;
const ROOT_UNIX: UnixAttributes = UnixAttributes {
    permissions: 0o755,
    uid: 0,
    gid: 0,
};
/// Every bit `chmod` sets (`S_IALLUGO` in `include/linux/stat.h`).
const S_IALLUGO: u32 = 0o7777;
const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
/// `XATTR_NAME_MAX` and `XATTR_SIZE_MAX` in `include/uapi/linux/limits.h`.
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65_536;

/// The Unix metadata an inode of a [`MemoryFs`] stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryUnixMetadata {
    pub kind: NodeKind,
    pub attributes: UnixAttributes,
    /// The device number a character or block device names; zero for
    /// every other kind.
    pub rdev: DeviceNumber,
}

/// How [`MemoryFs::set_xattr`] treats an attribute that already exists,
/// matching `XATTR_CREATE` and `XATTR_REPLACE` in
/// `include/uapi/linux/xattr.h`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XattrSetMode {
    /// Create or replace (`flags == 0`).
    #[default]
    Any,
    /// Fail with [`FsError::Exists`] when the attribute exists
    /// (`XATTR_CREATE`).
    Create,
    /// Fail with [`FsError::NoEntry`] when the attribute is absent
    /// (`XATTR_REPLACE`).
    Replace,
}

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
    unix: UnixAttributes,
    rdev: DeviceNumber,
    /// A file's bytes or a symbolic link's target text.
    contents: Bytes,
    xattrs: BTreeMap<String, Bytes>,
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
    /// An empty tree with only its root directory (`0755`, owned by
    /// `0:0`, as `RAMFS_DEFAULT_MODE` in `fs/ramfs/inode.c`).
    pub fn new(device: DeviceId) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_INODE,
            Inode {
                kind: NodeKind::Directory,
                unix: ROOT_UNIX,
                rdev: DeviceNumber::default(),
                contents: Bytes::new(),
                xattrs: BTreeMap::new(),
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
    /// times and Unix attributes: `contents` is a file's bytes or a link's
    /// target, and empty for a directory, which stays merging. When the
    /// tree has an object there already, another copy-up won and this one
    /// does nothing; when the layer below no longer shows there, the
    /// object was removed meanwhile: [`FsError::NoEntry`].
    pub(super) fn copy_up(
        &self,
        path: FsPath<'_>,
        kind: NodeKind,
        unix: UnixAttributes,
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
        let number = state.insert(
            path,
            kind,
            unix,
            DeviceNumber::default(),
            contents,
            original.modified_nanos,
        );
        let inode = state
            .inodes
            .get_mut(&number)
            .expect("the inode was just inserted");
        inode.access_nanos = original.access_nanos;
        inode.status_nanos = original.status_nanos;
        Ok(())
    }

    /// The Unix attributes and device number stored on the inode at
    /// `path`.
    pub fn unix_metadata(&self, path: FsPath<'_>) -> FsResult<MemoryUnixMetadata> {
        let state = self.state.lock();
        let (_, inode) = state.inode(path)?;
        Ok(MemoryUnixMetadata {
            kind: inode.kind,
            attributes: inode.unix,
            rdev: inode.rdev,
        })
    }

    /// Creates a regular file at `path` with explicit Unix attributes,
    /// keeping every bit of `07777` (the set-id and sticky bits included).
    pub fn create_file_with(
        &self,
        path: FsPath<'_>,
        unix: UnixAttributes,
        now: u64,
    ) -> FsResult<Metadata> {
        validate_permissions(unix.permissions)?;
        let mut state = self.state.lock();
        state.require_new_name(path)?;
        state.insert(
            path,
            NodeKind::File,
            unix,
            DeviceNumber::default(),
            Bytes::new(),
            now,
        );
        state.metadata(self.device, path)
    }

    /// Creates a directory at `path` with explicit Unix attributes,
    /// keeping every bit of `07777` (the set-id and sticky bits included).
    pub fn create_directory_with(
        &self,
        path: FsPath<'_>,
        unix: UnixAttributes,
        now: u64,
    ) -> FsResult<()> {
        validate_permissions(unix.permissions)?;
        let mut state = self.state.lock();
        state.require_new_name(path)?;
        state.insert(
            path,
            NodeKind::Directory,
            unix,
            DeviceNumber::default(),
            Bytes::new(),
            now,
        );
        Ok(())
    }

    /// Creates a symbolic link at `link` to `target` with explicit Unix
    /// attributes.
    pub fn symlink_with(
        &self,
        target: &str,
        link: FsPath<'_>,
        unix: UnixAttributes,
        now: u64,
    ) -> FsResult<()> {
        validate_permissions(unix.permissions)?;
        let mut state = self.state.lock();
        state.require_new_name(link)?;
        state.insert(
            link,
            NodeKind::Symlink,
            unix,
            DeviceNumber::default(),
            Bytes::copy_from_slice(target.as_bytes()),
            now,
        );
        Ok(())
    }

    /// Creates a device node, a FIFO or a socket at `path`, as
    /// `shmem_mknod` in `mm/shmem.c`.
    pub fn create_special(
        &self,
        path: FsPath<'_>,
        special: Special,
        unix: UnixAttributes,
        now: u64,
    ) -> FsResult<()> {
        validate_permissions(unix.permissions)?;
        let (kind, rdev) = match special {
            Special::CharDevice(device) => (NodeKind::CharacterDevice, device),
            Special::BlockDevice(device) => (NodeKind::BlockDevice, device),
            Special::Fifo => (NodeKind::Fifo, DeviceNumber::default()),
            Special::Socket => (NodeKind::Socket, DeviceNumber::default()),
        };
        let mut state = self.state.lock();
        state.require_new_name(path)?;
        state.insert(path, kind, unix, rdev, Bytes::new(), now);
        Ok(())
    }

    /// Replaces the permission, set-id and sticky bits (`07777`) of the
    /// inode at `path`, keeping them on every kind of inode as Linux's
    /// storage does (`chmod_common` in `fs/open.c`).
    pub fn set_permissions(&self, path: FsPath<'_>, permissions: u32, now: u64) -> FsResult<()> {
        validate_permissions(permissions)?;
        let mut state = self.state.lock();
        let inode = state.inode_mut(path)?;
        inode.unix.permissions = permissions;
        inode.status_nanos = now;
        Ok(())
    }

    /// Sets the owner and/or the group of the inode at `path`, leaving
    /// either alone when `None`. On a non-directory, the set-id bits and
    /// `security.capability` are dropped (`chown_common` in `fs/open.c`
    /// with `ATTR_KILL_SUID | ATTR_KILL_SGID | ATTR_KILL_PRIV`).
    pub fn set_owner(
        &self,
        path: FsPath<'_>,
        uid: Option<u32>,
        gid: Option<u32>,
        now: u64,
    ) -> FsResult<()> {
        let mut state = self.state.lock();
        let inode = state.inode_mut(path)?;
        if let Some(uid) = uid {
            inode.unix.uid = uid;
        }
        if let Some(gid) = gid {
            inode.unix.gid = gid;
        }
        if inode.kind != NodeKind::Directory {
            inode.unix.permissions &= !(S_ISUID | S_ISGID);
            inode.xattrs.remove("security.capability");
        }
        inode.status_nanos = now;
        Ok(())
    }

    /// Reads the extended attribute `name` on the inode at `path`, or
    /// `Ok(None)` when that attribute is not set.
    pub fn xattr(&self, path: FsPath<'_>, name: &str) -> FsResult<Option<Bytes>> {
        let state = self.state.lock();
        let (_, inode) = state.inode(path)?;
        validate_xattr_name(inode.kind, name, false)?;
        Ok(inode.xattrs.get(name).cloned())
    }

    /// Lists every extended attribute name stored on the inode at `path`,
    /// in lexicographic order.
    pub fn list_xattrs(&self, path: FsPath<'_>) -> FsResult<Vec<String>> {
        let state = self.state.lock();
        let (_, inode) = state.inode(path)?;
        Ok(inode.xattrs.keys().cloned().collect())
    }

    /// Sets the extended attribute `name` on the inode at `path`, in the
    /// `user.*`, `security.*`, `trusted.*` and `system.posix_acl_*`
    /// namespaces of `shmem_xattr_handlers` in `mm/shmem.c`, with no
    /// policy refusal of what an attribute would mean.
    pub fn set_xattr(
        &self,
        path: FsPath<'_>,
        name: &str,
        value: Bytes,
        mode: XattrSetMode,
        now: u64,
    ) -> FsResult<()> {
        if value.len() > XATTR_SIZE_MAX {
            return Err(FsError::Overflow);
        }
        let mut state = self.state.lock();
        let inode = state.inode_mut(path)?;
        validate_xattr_name(inode.kind, name, true)?;
        let exists = inode.xattrs.contains_key(name);
        match (mode, exists) {
            (XattrSetMode::Create, true) => return Err(FsError::Exists),
            (XattrSetMode::Replace, false) => return Err(FsError::NoEntry),
            _ => {}
        }
        inode.xattrs.insert(String::from(name), value);
        inode.status_nanos = now;
        Ok(())
    }

    /// Removes the extended attribute `name` from the inode at `path`,
    /// returning whether it was present.
    pub fn remove_xattr(&self, path: FsPath<'_>, name: &str, now: u64) -> FsResult<bool> {
        let mut state = self.state.lock();
        let inode = state.inode_mut(path)?;
        validate_xattr_name(inode.kind, name, false)?;
        let removed = inode.xattrs.remove(name).is_some();
        if removed {
            inode.status_nanos = now;
        }
        Ok(removed)
    }

    /// Removes the object at `path` (a directory when `directory`, which
    /// must hold no names) and, when `whiteout`, records a whiteout there
    /// so the layer below stays hidden. With `whiteout`, a path the tree
    /// has no object at is the layer below's alone, and only the whiteout
    /// is recorded.
    pub(super) fn remove(
        &self,
        path: FsPath<'_>,
        directory: bool,
        whiteout: bool,
        now: u64,
    ) -> FsResult<()> {
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
                let number = state.inode_of(path).expect("removed name exists");
                let (parent, _) = path.split_last().expect("root removal was rejected");
                let parent = state.inode_of(parent).expect("removed name has a parent");
                state
                    .inodes
                    .get_mut(&number)
                    .expect("removed inode exists")
                    .status_nanos = now;
                let parent = state.inodes.get_mut(&parent).expect("parent inode exists");
                parent.modified_nanos = now;
                parent.status_nanos = now;
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
    /// An exchange leaves both names in place and so records no whiteout.
    ///
    /// The whole change happens under the tree's lock, so it is atomic to
    /// every reader.
    pub(super) fn rename_over(
        &self,
        source: FsPath<'_>,
        destination: FsPath<'_>,
        mode: RenameMode,
        now: u64,
        whiteout: bool,
    ) -> FsResult<()> {
        if source.is_root() || destination.is_root() {
            return Err(FsError::NotPermitted);
        }
        let mut state = self.state.lock();
        let (_, inode) = state.inode(source)?;
        if mode == RenameMode::Exchange {
            debug_assert!(!whiteout, "an exchange hides nothing");
            return state.exchange(source, destination, now);
        }
        if source == destination {
            return match mode {
                RenameMode::NoReplace => Err(FsError::Exists),
                RenameMode::Replace | RenameMode::Exchange => Ok(()),
            };
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
        let renamed = |name: &str| rebase(name, source, destination);
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

/// Validates `permissions` against `07777` (`S_IALLUGO`).
fn validate_permissions(permissions: u32) -> FsResult<()> {
    if permissions & !S_IALLUGO != 0 {
        return Err(FsError::Invalid);
    }
    Ok(())
}

/// Validates an extended attribute name against Linux's tmpfs handlers
/// (`shmem_xattr_handlers` in `mm/shmem.c`, `xattr_resolve_name` and
/// `xattr_permission` in `fs/xattr.c`, and `posix_acl_xattr_set` in
/// `fs/posix_acl.c`). No attribute is refused for what its value means.
fn validate_xattr_name(kind: NodeKind, name: &str, writing: bool) -> FsResult<()> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(FsError::Overflow);
    }
    if let Some(suffix) = name.strip_prefix("user.") {
        if suffix.is_empty() {
            return Err(FsError::Invalid);
        }
        if !matches!(kind, NodeKind::File | NodeKind::Directory) {
            return Err(FsError::NotPermitted);
        }
        return Ok(());
    }
    if let Some(suffix) = name
        .strip_prefix("security.")
        .or_else(|| name.strip_prefix("trusted."))
    {
        if suffix.is_empty() {
            return Err(FsError::Invalid);
        }
        return Ok(());
    }
    if name == "system.posix_acl_access" {
        if kind == NodeKind::Symlink {
            return Err(FsError::Unsupported);
        }
        return Ok(());
    }
    if name == "system.posix_acl_default" {
        if kind == NodeKind::Symlink {
            return Err(FsError::Unsupported);
        }
        if writing && kind != NodeKind::Directory {
            return Err(FsError::NotPermitted);
        }
        return Ok(());
    }
    Err(FsError::Unsupported)
}

/// `name`, which is `from` or lies below it, moved to the same place
/// below `to`.
fn rebase(name: &str, from: FsPath<'_>, to: FsPath<'_>) -> String {
    let mut rebased = String::with_capacity(to.as_str().len() + name.len() - from.as_str().len());
    rebased.push_str(to.as_str());
    rebased.push_str(&name[from.as_str().len()..]);
    rebased
}

impl MemoryState {
    /// Swaps the objects at `first` and `second`, each with whatever lies
    /// below it. Two names of one object stay as they are, and a directory
    /// with the other name below it is [`FsError::Invalid`].
    fn exchange(&mut self, first: FsPath<'_>, second: FsPath<'_>, now: u64) -> FsResult<()> {
        let (first_number, first_kind) = {
            let (number, inode) = self.inode(first)?;
            (number, inode.kind)
        };
        let (second_number, second_kind) = {
            let (number, inode) = self.inode(second)?;
            (number, inode.kind)
        };
        if first_number == second_number {
            return Ok(());
        }
        if (first_kind == NodeKind::Directory && second.starts_with(first))
            || (second_kind == NodeKind::Directory && first.starts_with(second))
        {
            return Err(FsError::Invalid);
        }
        let subtree = |state: &Self, path: FsPath<'_>, number: u64| -> Vec<(String, u64)> {
            core::iter::once((path.as_str().to_string(), number))
                .chain(
                    state
                        .descendants(path)
                        .map(|(name, number)| (name.clone(), *number)),
                )
                .collect()
        };
        let first_names = subtree(self, first, first_number);
        let second_names = subtree(self, second, second_number);
        let first_whiteouts = self.whiteouts_below(first);
        let second_whiteouts = self.whiteouts_below(second);
        for (name, _) in first_names.iter().chain(&second_names) {
            self.names.remove(name);
        }
        for name in first_whiteouts.iter().chain(&second_whiteouts) {
            self.whiteouts.remove(name);
        }
        for (name, number) in first_names {
            self.names.insert(rebase(&name, first, second), number);
        }
        for (name, number) in second_names {
            self.names.insert(rebase(&name, second, first), number);
        }
        for name in first_whiteouts {
            self.whiteouts.insert(rebase(&name, first, second));
        }
        for name in second_whiteouts {
            self.whiteouts.insert(rebase(&name, second, first));
        }
        for number in [first_number, second_number] {
            self.inodes
                .get_mut(&number)
                .expect("every name refers to a live inode")
                .status_nanos = now;
        }
        for path in [first, second] {
            let (parent, _) = path.split_last().expect("the root was rejected");
            let parent = self.inode_mut(parent)?;
            parent.modified_nanos = now;
            parent.status_nanos = now;
        }
        Ok(())
    }

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
                NodeKind::File | NodeKind::Symlink => inode.contents.len() as u64,
                // The tree creates only files, directories and symlinks;
                // a report-only node has no contents here.
                NodeKind::Directory
                | NodeKind::CharacterDevice
                | NodeKind::BlockDevice
                | NodeKind::Fifo
                | NodeKind::Socket => 0,
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
            NodeKind::File
            | NodeKind::Symlink
            | NodeKind::CharacterDevice
            | NodeKind::BlockDevice
            | NodeKind::Fifo
            | NodeKind::Socket => Err(FsError::NotDirectory),
        }
    }

    /// The Unix attributes a node of `kind` at `path` inherits from its
    /// parent directory when created without a caller identity.
    fn inherited_unix(&self, path: FsPath<'_>, kind: NodeKind) -> FsResult<UnixAttributes> {
        let (parent, _) = path.split_last().ok_or(FsError::Exists)?;
        Ok(self.inode(parent)?.1.unix.inherited(kind))
    }

    fn insert(
        &mut self,
        path: FsPath<'_>,
        kind: NodeKind,
        unix: UnixAttributes,
        rdev: DeviceNumber,
        contents: Bytes,
        now: u64,
    ) -> u64 {
        let number = self.next_inode;
        self.next_inode += 1;
        self.inodes.insert(
            number,
            Inode {
                kind,
                unix,
                rdev,
                contents,
                xattrs: BTreeMap::new(),
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
        inode.kind.require_regular()?;
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

/// A write to a [`MemoryFs`], staged at once: its one round is the whole
/// write, and filling writes straight into the file's
/// own storage under the tree's lock.
pub struct MemoryWrite {
    fs: MemoryFs,
    path: OwnedFsPath,
    position: WritePosition,
    len: usize,
    now: u64,
}

impl MemoryWrite {
    /// Stages a write of `len` bytes to the regular file at `path`,
    /// refusing it at once when `path` names none.
    pub(super) fn prepare(
        fs: &MemoryFs,
        path: FsPath<'_>,
        position: WritePosition,
        len: usize,
        now: u64,
    ) -> FsResult<Self> {
        fs.state.lock().inode(path)?.1.kind.require_regular()?;
        Ok(Self {
            fs: fs.clone(),
            path: path.into(),
            position,
            len,
            now,
        })
    }

    /// Lands the write and resolves with the offset it began at. The
    /// checks of [`Self::prepare`] are made again under the lock, since
    /// the file may have changed in between.
    pub(super) fn land(self, mut fill: impl FnMut(usize, &mut [u8])) -> FsResult<Landed> {
        self.fs.write_now(
            self.path.as_path(),
            self.position,
            self.len,
            self.now,
            |destination| fill(0, destination),
        )
    }
}

impl PreparedWrite for MemoryWrite {
    type Filled = FsResult<WriteProgress<Self>>;

    fn staged_len(&self) -> usize {
        self.len
    }

    fn fill<F>(self, fill: F) -> Self::Filled
    where
        F: FnMut(usize, &mut [u8]),
    {
        // The result retains neither the filler nor the tree lock.
        self.land(fill).map(WriteProgress::Done)
    }
}

impl MemoryFs {
    /// Writes `len` bytes at `position` at once: `fill` writes straight
    /// into the file's storage.
    pub(super) fn write_now(
        &self,
        path: FsPath<'_>,
        position: WritePosition,
        len: usize,
        now: u64,
        fill: impl FnOnce(&mut [u8]),
    ) -> FsResult<Landed> {
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
        Ok(Landed {
            offset: landed,
            len,
        })
    }
}

impl FileSystem for MemoryFs {
    type Write = MemoryWrite;

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
                if let Some(refusal) = inode.kind.open_refusal() {
                    return Err(refusal);
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
                let unix = state.inherited_unix(path, NodeKind::File)?;
                state.insert(
                    path,
                    NodeKind::File,
                    unix,
                    DeviceNumber::default(),
                    Bytes::new(),
                    now,
                );
            }
            Err(error) => return Err(error),
        }
        state.metadata(self.device, path)
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        let state = self.state.lock();
        let (_, inode) = state.inode(path)?;
        inode.kind.require_regular()?;
        let offset = usize::try_from(offset).map_err(|_| FsError::Overflow)?;
        if offset >= inode.contents.len() {
            return Ok(Bytes::new());
        }
        let end = offset.saturating_add(max_bytes).min(inode.contents.len());
        Ok(inode.contents.slice(offset..end))
    }

    fn prepare_write<'a>(
        &'a self,
        path: FsPath<'a>,
        position: WritePosition,
        len: usize,
        now: u64,
    ) -> impl Future<Output = FsResult<MemoryWrite>> + Send + 'a {
        core::future::ready(MemoryWrite::prepare(self, path, position, len, now))
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
        let unix = state.inherited_unix(path, NodeKind::Directory)?;
        state.insert(
            path,
            NodeKind::Directory,
            unix,
            DeviceNumber::default(),
            Bytes::new(),
            now,
        );
        Ok(())
    }

    async fn remove_directory(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.remove(path, true, false, now)
    }

    async fn unlink(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.remove(path, false, false, now)
    }

    async fn rename(
        &self,
        source: FsPath<'_>,
        destination: FsPath<'_>,
        mode: RenameMode,
        now: u64,
    ) -> FsResult<()> {
        self.rename_over(source, destination, mode, now, false)
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
        let unix = state.inherited_unix(link, NodeKind::Symlink)?;
        state.insert(
            link,
            NodeKind::Symlink,
            unix,
            DeviceNumber::default(),
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
        DeviceId, DirCookie, FileSystem, FilledWrite, FsError, FsPath, Landed, NodeKind, OpenFlags,
        PreparedWrite, RenameMode, WritePosition, WriteProgress,
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
    fn a_write_is_refused_while_staging_and_a_dropped_one_changes_nothing() {
        let fs = tree();
        block_on(fs.create_directory(path("d"), 1)).expect("creates the directory");
        block_on(fs.open(path("f"), OpenFlags::CREATE, 1)).expect("creates the file");
        block_on(write_bytes(
            &fs,
            path("f"),
            WritePosition::At(0),
            b"kept",
            2,
        ))
        .expect("writes");

        assert!(matches!(
            block_on(fs.prepare_write(path("d"), WritePosition::At(0), 1, 3)),
            Err(FsError::IsDirectory)
        ));
        assert!(matches!(
            block_on(fs.prepare_write(path("missing"), WritePosition::End, 1, 3)),
            Err(FsError::NoEntry)
        ));
        // Staged, then dropped at the end of the statement.
        assert_eq!(
            block_on(fs.prepare_write(path("f"), WritePosition::At(0), 4, 3))
                .expect("stages")
                .staged_len(),
            4
        );

        let read = block_on(fs.read_at(path("f"), 0, 64)).expect("reads");
        assert_eq!(read.as_ref(), b"kept");
        assert_eq!(
            block_on(fs.stat(path("f"))).expect("stats").modified_nanos,
            2
        );
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
        // The filesystem stages the whole write in one round and hands its
        // own storage to the filler.
        let staged =
            block_on(fs.prepare_write(path("f"), WritePosition::End, 2, 7)).expect("stages");
        assert_eq!(staged.staged_len(), 2);
        assert!(matches!(
            block_on(
                staged
                    .fill(|at, destination| {
                        assert_eq!((at, &*destination), (0, &[0, 0][..]));
                        destination.copy_from_slice(b"ef");
                    })
                    .submit()
            ),
            Ok(WriteProgress::Done(Landed { offset: 4, len: 2 }))
        ));
        let read = block_on(fs.read_at(path("f"), 0, 64)).expect("reads");
        assert_eq!(read.as_ref(), b"\0\0cdef");
        let stat = block_on(fs.stat(path("f"))).expect("stats");
        assert_eq!((stat.size, stat.modified_nanos), (6, 7));
        block_on(fs.open(path("f"), OpenFlags::TRUNCATE, 8)).expect("truncates");
        assert_eq!(block_on(fs.stat(path("f"))).expect("stats").size, 0);
    }

    #[test]
    fn a_filled_write_outlives_its_source() {
        let fs = tree();
        block_on(fs.open(path("f"), OpenFlags::CREATE, 0)).expect("creates");
        let source = "文件・ファイル・파일".as_bytes().to_vec();
        let staged = {
            let preparing_fs = fs.clone();
            let preparing_path = String::from("f");
            block_on(preparing_fs.prepare_write(
                path(&preparing_path),
                WritePosition::At(0),
                source.len(),
                1,
            ))
            .expect("stages")
        };
        let filled = staged.fill(|at, destination| {
            destination.copy_from_slice(&source[at..at + destination.len()]);
        });
        let len = source.len();
        drop(source);
        assert!(matches!(
            block_on(filled.submit()),
            Ok(WriteProgress::Done(Landed { offset: 0, len: landed })) if landed == len
        ));
        let bytes = block_on(fs.read_at(path("f"), 0, 128)).expect("reads");
        assert_eq!(bytes.as_ref(), "文件・ファイル・파일".as_bytes());
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
        block_on(fs.unlink(path("a"), 7_000_000_000)).expect("unlinks");
        assert_eq!(block_on(fs.stat(path("b"))).expect("stats").link_count, 1);
    }

    #[test]
    fn rename_noreplace_refuses_any_destination_and_exchange_needs_both() {
        let fs = tree();
        block_on(fs.create_directory(path("d"), 0)).expect("creates");
        block_on(fs.open(path("f"), OpenFlags::CREATE, 0)).expect("creates");
        for (source, destination) in [("d", "f"), ("f", "d"), ("f", "f")] {
            assert_eq!(
                block_on(fs.rename(path(source), path(destination), RenameMode::NoReplace, 1)),
                Err(FsError::Exists),
                "{source} over {destination}"
            );
        }
        assert_eq!(
            block_on(fs.rename(path("none"), path("f"), RenameMode::NoReplace, 1)),
            Err(FsError::NoEntry)
        );
        block_on(fs.rename(path("f"), path("g"), RenameMode::NoReplace, 1)).expect("renames");
        assert_eq!(names(&fs, ""), ["d", "g"]);
        for (source, destination) in [("g", "none"), ("none", "g"), ("none", "never")] {
            assert_eq!(
                block_on(fs.rename(path(source), path(destination), RenameMode::Exchange, 1)),
                Err(FsError::NoEntry),
                "{source} with {destination}"
            );
        }
    }

    #[test]
    fn rename_exchange_swaps_subtrees_and_refuses_an_ancestor() {
        let fs = tree();
        block_on(fs.create_directory(path("a"), 0)).expect("creates");
        block_on(fs.open(path("a/inner"), OpenFlags::CREATE, 0)).expect("creates");
        block_on(fs.create_directory(path("b"), 0)).expect("creates");
        block_on(fs.create_directory(path("b/deeper"), 0)).expect("creates");
        block_on(fs.open(path("b/deeper/leaf"), OpenFlags::CREATE, 0)).expect("creates");
        block_on(fs.open(path("file"), OpenFlags::CREATE, 0)).expect("creates");
        let identity = |name: &str| block_on(fs.stat(path(name))).expect("stats").id;
        let (a, b, file) = (identity("a"), identity("b"), identity("file"));
        assert_eq!(
            block_on(fs.rename(path("a"), path("a/inner"), RenameMode::Exchange, 1)),
            Err(FsError::Invalid)
        );
        assert_eq!(
            block_on(fs.rename(path("b/deeper"), path("b"), RenameMode::Exchange, 1)),
            Err(FsError::Invalid)
        );
        block_on(fs.rename(path("a"), path("b"), RenameMode::Exchange, 2)).expect("exchanges");
        assert_eq!((identity("a"), identity("b")), (b, a));
        assert_eq!(names(&fs, "a"), ["deeper"]);
        assert_eq!(names(&fs, "a/deeper"), ["leaf"]);
        assert_eq!(names(&fs, "b"), ["inner"]);
        // A file against a directory, and an object against itself.
        block_on(fs.rename(path("file"), path("a/deeper"), RenameMode::Exchange, 3))
            .expect("exchanges");
        assert_eq!(names(&fs, "file"), ["leaf"]);
        assert_eq!(
            block_on(fs.stat(path("a/deeper"))).expect("stats").kind,
            NodeKind::File
        );
        assert_eq!(identity("a/deeper"), file);
        block_on(fs.rename(path("b"), path("b"), RenameMode::Exchange, 4)).expect("a no-op");
        assert_eq!(names(&fs, "b"), ["inner"]);
        assert_eq!(names(&fs, ""), ["a", "b", "file"]);
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
            block_on(fs.rename(path("d"), path("d/inner"), RenameMode::Replace, 1)),
            Err(FsError::NotPermitted)
        );
        block_on(fs.rename(path("d"), path("e"), RenameMode::Replace, 1)).expect("renames");
        assert_eq!(names(&fs, ""), ["d.x", "dx", "e"]);
        assert_eq!(names(&fs, "e"), ["f"]);
        assert_eq!(
            block_on(fs.remove_directory(path("e"), 7_000_000_000)),
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

    #[test]
    fn a_node_created_without_an_identity_inherits_from_its_parent() {
        use crate::UnixAttributes;

        let fs = tree();
        let directories = [
            ("中文目录", 0o2775, 1000, 100),
            ("日本語のディレクトリ", 0o1777, 0, 0),
            ("한국어디렉터리", 0o4750, 7, 8),
        ];
        for (name, permissions, uid, gid) in directories {
            fs.create_directory_with(
                path(name),
                UnixAttributes {
                    permissions,
                    uid,
                    gid,
                },
                10,
            )
            .expect("creates directory");
            let file = alloc::format!("{name}/ファイル");
            block_on(fs.open(path(&file), OpenFlags::CREATE, 20)).expect("creates file");
            let child = alloc::format!("{name}/하위");
            block_on(fs.create_directory(path(&child), 20)).expect("creates subdirectory");
            let link = alloc::format!("{name}/链接");
            block_on(fs.symlink("ファイル", path(&link), 20)).expect("creates symlink");

            assert_eq!(
                fs.unix_metadata(path(&file)).expect("file").attributes,
                UnixAttributes {
                    permissions: permissions & 0o666,
                    uid,
                    gid,
                },
                "{name}"
            );
            assert_eq!(
                fs.unix_metadata(path(&child)).expect("child").attributes,
                UnixAttributes {
                    permissions: permissions & 0o2777,
                    uid,
                    gid,
                },
                "{name}"
            );
            assert_eq!(
                fs.unix_metadata(path(&link)).expect("link").attributes,
                UnixAttributes {
                    permissions: 0o777,
                    uid,
                    gid,
                },
                "{name}"
            );
        }
    }

    #[test]
    fn stores_unix_authority_device_nodes_and_xattrs_as_linux_does() {
        use bytes::Bytes;
        use helios_btrfs::DeviceNumber;
        use helios_btrfs::write::Special;

        use super::XattrSetMode;
        use crate::UnixAttributes;

        let fs = tree();
        let owner = |permissions| UnixAttributes {
            permissions,
            uid: 1000,
            gid: 100,
        };
        assert_eq!(
            fs.create_file_with(path("bad"), owner(0o100_644), 1)
                .map(drop),
            Err(FsError::Invalid)
        );
        fs.create_file_with(path("suid"), owner(0o4755), 1)
            .expect("creates setuid file");
        fs.create_directory_with(path("sgid"), owner(0o3775), 1)
            .expect("creates setgid sticky directory");
        fs.symlink_with("suid", path("link"), owner(0o777), 1)
            .expect("creates symlink");

        let null = DeviceNumber::new(1, 3).unwrap();
        let wide = DeviceNumber::new(259, 0x1_2345).unwrap();
        for (name, special, kind, expected_rdev, open_err) in [
            (
                "chr",
                Special::CharDevice(null),
                NodeKind::CharacterDevice,
                null,
                FsError::NotPermitted,
            ),
            (
                "blk",
                Special::BlockDevice(wide),
                NodeKind::BlockDevice,
                wide,
                FsError::NotPermitted,
            ),
            (
                "fifo",
                Special::Fifo,
                NodeKind::Fifo,
                DeviceNumber::default(),
                FsError::Unsupported,
            ),
            (
                "sock",
                Special::Socket,
                NodeKind::Socket,
                DeviceNumber::default(),
                FsError::Unsupported,
            ),
        ] {
            fs.create_special(path(name), special, owner(0o6640), 2)
                .expect("creates special");
            let meta = fs.unix_metadata(path(name)).expect("special metadata");
            assert_eq!(meta.kind, kind, "{name}");
            assert_eq!(meta.rdev, expected_rdev, "{name}");
            assert_eq!(meta.attributes, owner(0o6640), "{name}");
            assert_eq!(
                block_on(fs.open(path(name), OpenFlags::empty(), 3)).map(drop),
                Err(open_err),
                "{name}"
            );
        }

        // Extended attributes across every handler Linux's tmpfs keeps, with
        // no policy refusal of `security.*`, `trusted.*` or POSIX ACLs.
        for (attr, value) in [
            ("user.comment", &b"hello"[..]),
            ("security.capability", &[1, 0, 0, 2][..]),
            ("trusted.overlay.opaque", b"y"),
            ("system.posix_acl_access", &[2, 0, 0, 0][..]),
        ] {
            fs.set_xattr(
                path("suid"),
                attr,
                Bytes::copy_from_slice(value),
                XattrSetMode::Create,
                4,
            )
            .expect(attr);
            assert_eq!(
                fs.set_xattr(
                    path("suid"),
                    attr,
                    Bytes::from_static(b"dup"),
                    XattrSetMode::Create,
                    5,
                ),
                Err(FsError::Exists)
            );
        }
        fs.set_xattr(
            path("sgid"),
            "system.posix_acl_default",
            Bytes::from_static(&[2, 0, 0, 0]),
            XattrSetMode::Any,
            4,
        )
        .expect("default ACL on directory");
        assert_eq!(
            fs.set_xattr(
                path("suid"),
                "system.posix_acl_default",
                Bytes::from_static(&[2, 0, 0, 0]),
                XattrSetMode::Any,
                4,
            ),
            Err(FsError::NotPermitted)
        );
        // `user.*` is refused on symlinks and special nodes (`xattr_permission`);
        // `security.*` and `trusted.*` are kept on them.
        assert_eq!(
            fs.set_xattr(
                path("chr"),
                "user.label",
                Bytes::from_static(b"no"),
                XattrSetMode::Any,
                4,
            ),
            Err(FsError::NotPermitted)
        );
        fs.set_xattr(
            path("chr"),
            "security.selinux",
            Bytes::from_static(b"system_u:object_r:null_device_t:s0"),
            XattrSetMode::Any,
            4,
        )
        .expect("security xattr on device node");
        fs.set_xattr(
            path("link"),
            "trusted.tag",
            Bytes::from_static(b"kept"),
            XattrSetMode::Any,
            4,
        )
        .expect("trusted xattr on symlink");
        assert_eq!(
            fs.set_xattr(
                path("suid"),
                "other.ns",
                Bytes::from_static(b"no"),
                XattrSetMode::Any,
                4,
            ),
            Err(FsError::Unsupported)
        );
        assert_eq!(
            fs.set_xattr(
                path("suid"),
                "user.missing",
                Bytes::from_static(b"no"),
                XattrSetMode::Replace,
                4,
            ),
            Err(FsError::NoEntry)
        );
        assert_eq!(
            fs.list_xattrs(path("suid")).unwrap(),
            [
                "security.capability",
                "system.posix_acl_access",
                "trusted.overlay.opaque",
                "user.comment",
            ]
        );

        // Unrelated writes, hard links and renames preserve stored metadata.
        block_on(write_bytes(
            &fs,
            path("suid"),
            WritePosition::At(0),
            b"#!/bin/sh\n",
            6,
        ))
        .expect("writes");
        block_on(fs.link(path("suid"), path("suid-hard"), 7)).expect("links");
        block_on(fs.rename(path("chr"), path("chr-moved"), RenameMode::Replace, 8))
            .expect("renames device node");
        assert_eq!(
            fs.unix_metadata(path("suid-hard")).unwrap().attributes,
            owner(0o4755)
        );
        assert_eq!(fs.unix_metadata(path("chr-moved")).unwrap().rdev, null);
        assert_eq!(
            fs.xattr(path("suid-hard"), "trusted.overlay.opaque")
                .unwrap()
                .as_deref(),
            Some(&b"y"[..])
        );

        // chmod stores every bit of 07777; chown on a non-directory drops
        // set-id bits and `security.capability`, while a directory keeps them.
        fs.set_permissions(path("suid"), 0o6755, 9).expect("chmod");
        assert_eq!(
            fs.unix_metadata(path("suid")).unwrap().attributes,
            owner(0o6755)
        );
        fs.set_owner(path("suid"), Some(0), Some(0), 10)
            .expect("chown file");
        assert_eq!(
            fs.unix_metadata(path("suid")).unwrap().attributes,
            UnixAttributes {
                permissions: 0o755,
                uid: 0,
                gid: 0,
            }
        );
        assert_eq!(fs.xattr(path("suid"), "security.capability").unwrap(), None);
        assert!(fs.remove_xattr(path("suid"), "user.comment", 11).unwrap());
        assert!(!fs.remove_xattr(path("suid"), "user.comment", 12).unwrap());

        fs.set_owner(path("sgid"), Some(0), Some(200), 13)
            .expect("chown directory");
        assert_eq!(
            fs.unix_metadata(path("sgid")).unwrap().attributes,
            UnixAttributes {
                permissions: 0o3775,
                uid: 0,
                gid: 200,
            }
        );
    }
}
