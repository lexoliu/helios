//! The writer, the inode map and every operation the daemon serves.
//!
//! As Linux's btrfs, a change is visible at once and durable at commit. Each
//! mutation joins the writer's running transaction, and reads go through the
//! writer's view of it, so the next request sees every reply's change. The
//! transaction commits on `fsync`, `fsyncdir` and unmount, and when its dirty
//! metadata reaches Linux's bound.

use std::ffi::OsStr;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{Errno, FileAttr, FileType, Generation, INodeNo, TimeOrNow};
use futures_lite::future::block_on;
use helios_btrfs::write::{
    Attributes, DEFAULT_DIRTY_METADATA_LIMIT, InodeHandle, Name, RootUuids, Special, WriteOptions,
    Writer,
};
use helios_btrfs::{
    DirPosition, EntryTarget, Error, FileKind, Filesystem, Inode, Metadata, Timestamp,
};
use helios_hal::fs::RenameMode;

use super::OpenError;
use super::device::Device;
use super::errno::{read_errno, write_errno};
use super::inodes::{Identity, Inodes};

/// The longest directory entry name, `BTRFS_NAME_LEN` in `btrfs_tree.h`.
const NAME_MAX: usize = 255;

/// The first inode number of a subvolume's tree is its root directory's,
/// `BTRFS_FIRST_FREE_OBJECTID` in `btrfs_tree.h`.
const SUBVOLUME_ROOT_INODE: u64 = 256;

/// A listing's first two cookies belong to `.` and `..`, which the format
/// does not store; an entry's cookie is its directory index plus this.
const DOT_COOKIES: u64 = 2;

/// `S_IALLUGO`: the permission, sticky and set-id bits of a mode.
const PERMISSION_BITS: u32 = 0o7777;

type Snapshot = Filesystem<Arc<Device>>;

/// The attributes a `setattr` request changes; `None` leaves one alone.
#[derive(Default)]
pub struct Setattr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub access: Option<TimeOrNow>,
    pub modified: Option<TimeOrNow>,
}

/// What a creation makes.
pub enum Node<'a> {
    File,
    Directory,
    Symlink(&'a [u8]),
    Special(Special),
}

/// The caller a creation is made for.
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

pub struct Statfs {
    pub blocks: u64,
    pub free: u64,
    pub block_size: u32,
}

pub struct State {
    writer: Writer<Device, RandomUuids>,
    inodes: Inodes,
    concurrency: NonZeroUsize,
    read_only: bool,
}

impl State {
    pub fn open(
        device: Device,
        concurrency: NonZeroUsize,
        read_only: bool,
    ) -> Result<Self, OpenError> {
        let mut writer = block_on(Writer::mount(device, RandomUuids, concurrency))?;
        let snapshot = writer.snapshot()?;
        let root = block_on(snapshot.root())?;
        let handle = {
            let transaction = block_on(writer.join())?;
            block_on(transaction.root())?
        };
        let identity = Identity {
            tree: root.tree(),
            number: root.number(),
        };
        Ok(Self {
            writer,
            inodes: Inodes::new(identity, root.metadata().generation, handle),
            concurrency,
            read_only,
        })
    }

    pub fn forget(&mut self, ino: INodeNo, count: u64) {
        self.inodes.forget(ino, count);
    }

    fn snapshot(&self) -> Result<Snapshot, Errno> {
        self.writer.snapshot().map_err(|error| write_errno(&error))
    }

    fn writable(&self) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        Ok(())
    }

    /// The inode `ino` names in `snapshot`; `ESTALE` once it was deleted or
    /// its number reused.
    async fn open_inode(&self, snapshot: &Snapshot, ino: INodeNo) -> Result<Inode, Errno> {
        let (identity, generation) = self.inodes.identity(ino)?;
        let inode = match snapshot.inode(identity.tree, identity.number).await {
            Ok(inode) => inode,
            Err(Error::MissingInode { .. }) => return Err(Errno::ESTALE),
            Err(error) => return Err(read_errno(&error)),
        };
        if generation.is_some_and(|generation| generation != inode.metadata().generation) {
            return Err(Errno::ESTALE);
        }
        Ok(inode)
    }

    /// The writer's handle for the entry `name` of directory `parent`.
    async fn handle_of(&mut self, parent: INodeNo, name: &[u8]) -> Result<InodeHandle, Errno> {
        let parent = self.inodes.handle(parent)?;
        let transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        transaction
            .lookup(&parent, name)
            .await
            .map_err(|error| write_errno(&error))?
            .ok_or(Errno::ENOENT)
    }

    /// Makes every change so far durable: `fsync`, `fsyncdir` and unmount.
    /// A failure poisons the writer, and the device keeps the previous
    /// generation.
    pub async fn sync(&mut self) -> Result<(), Errno> {
        self.writer
            .commit()
            .await
            .map_err(|error| write_errno(&error))
    }

    /// Ends a mutation: commits once the running transaction's dirty
    /// metadata reaches Linux's bound.
    async fn settle(&mut self) -> Result<(), Errno> {
        if self.writer.dirty().metadata_bytes >= DEFAULT_DIRTY_METADATA_LIMIT {
            self.sync().await?;
        }
        Ok(())
    }

    /// The attributes and generation of `identity` as the writer sees it,
    /// recorded as one more lookup of it.
    async fn entry_of(
        &mut self,
        identity: Identity,
        handle: Option<InodeHandle>,
    ) -> Result<(FileAttr, Generation), Errno> {
        let snapshot = self.snapshot()?;
        let inode = match snapshot.inode(identity.tree, identity.number).await {
            Ok(inode) => inode,
            Err(Error::MissingInode { .. }) => return Err(Errno::ESTALE),
            Err(error) => return Err(read_errno(&error)),
        };
        let generation = inode.metadata().generation;
        let ino = self.inodes.record(identity, generation, handle);
        Ok((
            attr(ino, inode.metadata(), &snapshot),
            Generation(generation),
        ))
    }

    pub async fn lookup(
        &mut self,
        parent: INodeNo,
        name: &OsStr,
    ) -> Result<(FileAttr, Generation), Errno> {
        let name = entry_name(name)?;
        let snapshot = self.snapshot()?;
        let directory = self.open_inode(&snapshot, parent).await?;
        let child = snapshot
            .lookup(&directory, name)
            .await
            .map_err(|error| read_errno(&error))?
            .ok_or(Errno::ENOENT)?;
        let identity = Identity {
            tree: child.tree(),
            number: child.number(),
        };
        let generation = child.metadata().generation;
        let handle = if self.inodes.has_handle(identity, generation) {
            None
        } else {
            Some(self.handle_of(parent, name).await?)
        };
        let ino = self.inodes.record(identity, generation, handle);
        Ok((
            attr(ino, child.metadata(), &snapshot),
            Generation(generation),
        ))
    }

    pub async fn getattr(&self, ino: INodeNo) -> Result<FileAttr, Errno> {
        let snapshot = self.snapshot()?;
        let inode = self.open_inode(&snapshot, ino).await?;
        Ok(attr(ino, inode.metadata(), &snapshot))
    }

    /// Applies a mode, owner, size and time change in one transaction.
    pub async fn setattr(
        &mut self,
        ino: INodeNo,
        change: Setattr,
        now: Timestamp,
    ) -> Result<FileAttr, Errno> {
        let Setattr {
            mode,
            uid,
            gid,
            size,
            access,
            modified,
        } = change;
        if mode.is_none()
            && uid.is_none()
            && gid.is_none()
            && size.is_none()
            && access.is_none()
            && modified.is_none()
        {
            return self.getattr(ino).await;
        }
        self.writable()?;
        let file = self.inodes.handle(ino)?;
        let concurrency = self.concurrency;
        let mut transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        // Linux applies a new owner before a new mode in one `notify_change`,
        // so a mode sent with it is the one that stays.
        if uid.is_some() || gid.is_some() {
            transaction
                .set_owner(&file, uid, gid, now)
                .await
                .map_err(|error| write_errno(&error))?;
        }
        if let Some(mode) = mode {
            // The kernel sends the file type with the mode; `chmod` keeps the
            // permission, sticky and set-id bits only.
            transaction
                .set_permissions(&file, mode & PERMISSION_BITS, now)
                .await
                .map_err(|error| write_errno(&error))?;
        }
        if let Some(size) = size {
            transaction
                .truncate_file(&file, size, now, concurrency)
                .await
                .map_err(|error| write_errno(&error))?;
        }
        if access.is_some() || modified.is_some() {
            let resolve = |time: TimeOrNow| match time {
                TimeOrNow::SpecificTime(time) => timestamp(time),
                TimeOrNow::Now => now,
            };
            transaction
                .set_times(&file, access.map(resolve), modified.map(resolve), now)
                .await
                .map_err(|error| write_errno(&error))?;
        }
        self.settle().await?;
        self.getattr(ino).await
    }

    pub async fn readlink(&self, ino: INodeNo) -> Result<Vec<u8>, Errno> {
        let snapshot = self.snapshot()?;
        let link = self.open_inode(&snapshot, ino).await?;
        // PATH_MAX; a longer target is read again at its full length.
        let mut target = vec![0; 4096];
        let length = snapshot
            .read_link(&link, &mut target)
            .await
            .map_err(|error| read_errno(&error))?;
        if length > target.len() {
            target.resize(length, 0);
            snapshot
                .read_link(&link, &mut target)
                .await
                .map_err(|error| read_errno(&error))?;
        }
        target.truncate(length);
        Ok(target)
    }

    pub async fn read(&self, ino: INodeNo, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let snapshot = self.snapshot()?;
        let file = self.open_inode(&snapshot, ino).await?;
        let mut data = vec![0; size as usize];
        let length = snapshot
            .read(&file, offset, &mut data)
            .await
            .map_err(|error| read_errno(&error))?;
        data.truncate(length);
        Ok(data)
    }

    /// Hands the entries of directory `ino` after cookie `offset` to `emit`
    /// until it returns true, meaning its reply is full.
    pub async fn readdir(
        &mut self,
        ino: INodeNo,
        offset: u64,
        mut emit: impl FnMut(INodeNo, u64, FileType, &OsStr) -> bool,
    ) -> Result<(), Errno> {
        let snapshot = self.snapshot()?;
        let directory = self.open_inode(&snapshot, ino).await?;
        // The format stores neither `.` nor `..`; the kernel reports both.
        // The parent is not recorded, so `..` carries the directory's own number.
        for (cookie, name) in [(1, "."), (2, "..")] {
            if offset < cookie && emit(ino, cookie, FileType::Directory, OsStr::new(name)) {
                return Ok(());
            }
        }
        let tree = directory.tree();
        let inodes = &mut self.inodes;
        snapshot
            .read_dir(
                &directory,
                DirPosition(offset.saturating_sub(DOT_COOKIES)),
                |entry| {
                    let identity = match entry.target {
                        EntryTarget::Inode(number) => Identity { tree, number },
                        EntryTarget::Subvolume(tree) => Identity {
                            tree,
                            number: SUBVOLUME_ROOT_INODE,
                        },
                    };
                    let listed = inodes.listed(identity);
                    let cookie = entry.position.0.saturating_add(DOT_COOKIES + 1);
                    if emit(
                        listed,
                        cookie,
                        file_type(entry.kind),
                        OsStr::from_bytes(entry.name),
                    ) {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            )
            .await
            .map_err(|error| read_errno(&error))?;
        Ok(())
    }

    pub async fn write(
        &mut self,
        ino: INodeNo,
        offset: u64,
        data: &[u8],
        now: Timestamp,
    ) -> Result<u32, Errno> {
        self.writable()?;
        let file = self.inodes.handle(ino)?;
        let concurrency = self.concurrency;
        let mut transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        let written = transaction
            .write_file(&file, offset, data, WriteOptions::plain(now, concurrency))
            .await
            .map_err(|error| write_errno(&error))?;
        self.settle().await?;
        Ok(u32::try_from(written)
            .expect("a write is never longer than the request that asked for it"))
    }

    pub async fn create(
        &mut self,
        parent: INodeNo,
        name: &OsStr,
        node: Node<'_>,
        permissions: u32,
        owner: Owner,
        now: Timestamp,
    ) -> Result<(FileAttr, Generation), Errno> {
        self.writable()?;
        let name = entry_name(name)?;
        let parent = self.inodes.handle(parent)?;
        let mut transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        let directory = transaction
            .metadata(&parent)
            .await
            .map_err(|error| write_errno(&error))?;
        // `inode_init_owner` in `fs/inode.c`: in a set-group-id directory a
        // new entry takes the directory's group, and a new directory keeps
        // S_ISGID too.
        let (permissions, gid) = if directory.permissions & 0o2000 != 0 {
            let inherited = if matches!(node, Node::Directory) {
                permissions | 0o2000
            } else {
                permissions
            };
            (inherited, directory.gid)
        } else {
            (permissions, owner.gid)
        };
        let at = Name {
            parent: &parent,
            bytes: name,
        };
        let attributes = Attributes {
            permissions,
            uid: owner.uid,
            gid,
        };
        let created = match node {
            Node::File => transaction.create_file(at, attributes, now).await,
            Node::Directory => transaction.create_directory(at, attributes, now).await,
            Node::Symlink(target) => transaction.symlink(at, target, attributes, now).await,
            Node::Special(special) => {
                transaction
                    .create_special(at, special, attributes, now)
                    .await
            }
        }
        .map_err(|error| write_errno(&error))?;
        self.settle().await?;
        let identity = Identity {
            tree: created.tree(),
            number: created.number(),
        };
        self.entry_of(identity, Some(created)).await
    }

    pub async fn link(
        &mut self,
        ino: INodeNo,
        parent: INodeNo,
        name: &OsStr,
        now: Timestamp,
    ) -> Result<(FileAttr, Generation), Errno> {
        self.writable()?;
        let name = entry_name(name)?;
        let file = self.inodes.handle(ino)?;
        let parent = self.inodes.handle(parent)?;
        let (identity, _) = self.inodes.identity(ino)?;
        let mut transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        transaction
            .hard_link(
                Name {
                    parent: &parent,
                    bytes: name,
                },
                &file,
                now,
            )
            .await
            .map_err(|error| write_errno(&error))?;
        self.settle().await?;
        self.entry_of(identity, None).await
    }

    /// Removes the entry `name` of `parent`: a directory for `rmdir`, any
    /// other kind for `unlink`.
    pub async fn remove(
        &mut self,
        parent: INodeNo,
        name: &OsStr,
        directory: bool,
        now: Timestamp,
    ) -> Result<(), Errno> {
        self.writable()?;
        let name = entry_name(name)?;
        let parent = self.inodes.handle(parent)?;
        let mut transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        let child = transaction
            .lookup(&parent, name)
            .await
            .map_err(|error| write_errno(&error))?
            .ok_or(Errno::ENOENT)?;
        let kind = transaction
            .metadata(&child)
            .await
            .map_err(|error| write_errno(&error))?
            .kind;
        match (directory, kind == FileKind::Directory) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            _ => {}
        }
        transaction
            .unlink_name(
                Name {
                    parent: &parent,
                    bytes: name,
                },
                now,
            )
            .await
            .map_err(|error| write_errno(&error))?;
        self.settle().await
    }

    pub async fn rename(
        &mut self,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        mode: RenameMode,
        now: Timestamp,
    ) -> Result<(), Errno> {
        self.writable()?;
        let name = entry_name(name)?;
        let new_name = entry_name(new_name)?;
        let parent = self.inodes.handle(parent)?;
        let new_parent = self.inodes.handle(new_parent)?;
        let mut transaction = self
            .writer
            .join()
            .await
            .map_err(|error| write_errno(&error))?;
        transaction
            .rename(
                Name {
                    parent: &parent,
                    bytes: name,
                },
                Name {
                    parent: &new_parent,
                    bytes: new_name,
                },
                mode,
                now,
            )
            .await
            .map_err(|error| write_errno(&error))?;
        self.settle().await
    }

    /// The space as the running transaction leaves it: a change is counted
    /// as soon as it is visible, not once it commits.
    pub fn statfs(&self) -> Result<Statfs, Errno> {
        let block_size = self.snapshot()?.superblock().sectorsize();
        let usage = self.writer.usage().map_err(|error| write_errno(&error))?;
        let size = u64::from(block_size);
        Ok(Statfs {
            blocks: usage.total_bytes / size,
            free: usage.total_bytes.saturating_sub(usage.used_bytes) / size,
            block_size,
        })
    }

    /// The value of extended attribute `name`; its full length even when
    /// `size` is too small to hold it, and nothing when `size` is zero.
    pub async fn getxattr(
        &self,
        ino: INodeNo,
        name: &OsStr,
        size: u32,
    ) -> Result<(usize, Vec<u8>), Errno> {
        let snapshot = self.snapshot()?;
        let inode = self.open_inode(&snapshot, ino).await?;
        let mut value = vec![0; size as usize];
        let length = snapshot
            .xattr(&inode, name.as_bytes(), &mut value)
            .await
            .map_err(|error| read_errno(&error))?
            .ok_or(Errno::NO_XATTR)?;
        value.truncate(length.min(value.len()));
        Ok((length, value))
    }

    /// The names of the extended attributes, each followed by a NUL.
    pub async fn listxattr(&self, ino: INodeNo) -> Result<Vec<u8>, Errno> {
        let snapshot = self.snapshot()?;
        let inode = self.open_inode(&snapshot, ino).await?;
        let mut names = Vec::new();
        snapshot
            .list_xattrs(&inode, |name| {
                names.extend_from_slice(name);
                names.push(0);
                ControlFlow::Continue(())
            })
            .await
            .map_err(|error| read_errno(&error))?;
        Ok(names)
    }
}

/// A directory entry name as bytes.
fn entry_name(name: &OsStr) -> Result<&[u8], Errno> {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok(bytes)
}

/// The target of a symlink as bytes.
pub fn target_bytes(target: &Path) -> &[u8] {
    target.as_os_str().as_bytes()
}

/// Random version 4 UUIDs for the root records that lack one.
struct RandomUuids;

impl RootUuids for RandomUuids {
    fn next_uuid(&mut self) -> [u8; 16] {
        let mut uuid = [0; 16];
        getrandom::fill(&mut uuid).expect("the operating system's random source failed");
        // RFC 9562: version 4 and the RFC variant.
        uuid[6] = (uuid[6] & 0x0f) | 0x40;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        uuid
    }
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::Regular => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
        FileKind::CharDevice => FileType::CharDevice,
        FileKind::BlockDevice => FileType::BlockDevice,
        FileKind::Fifo => FileType::NamedPipe,
        FileKind::Socket => FileType::Socket,
    }
}

fn system_time(time: Timestamp) -> SystemTime {
    let nanoseconds = Duration::from_nanos(u64::from(time.nanoseconds));
    let seconds = Duration::from_secs(time.seconds.unsigned_abs());
    if time.seconds >= 0 {
        UNIX_EPOCH + seconds + nanoseconds
    } else {
        UNIX_EPOCH - seconds + nanoseconds
    }
}

pub fn timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => Timestamp {
            seconds: since.as_secs() as i64,
            nanoseconds: since.subsec_nanos(),
        },
        Err(error) => {
            let before = error.duration();
            let whole = -(before.as_secs() as i64);
            match before.subsec_nanos() {
                0 => Timestamp {
                    seconds: whole,
                    nanoseconds: 0,
                },
                nanoseconds => Timestamp {
                    seconds: whole - 1,
                    nanoseconds: 1_000_000_000 - nanoseconds,
                },
            }
        }
    }
}

fn attr(ino: INodeNo, metadata: &Metadata, snapshot: &Snapshot) -> FileAttr {
    FileAttr {
        ino,
        size: metadata.size,
        blocks: metadata.nbytes.div_ceil(512),
        atime: system_time(metadata.atime),
        mtime: system_time(metadata.mtime),
        ctime: system_time(metadata.ctime),
        crtime: system_time(metadata.otime),
        kind: file_type(metadata.kind),
        // The permission and set-id bits, which fit 12 bits.
        perm: metadata.permissions as u16,
        nlink: metadata.nlink,
        uid: metadata.uid,
        gid: metadata.gid,
        // FUSE carries `stat`'s encoding, which the kernel decodes with
        // `new_decode_dev` (`fuse_fillattr` in `fs/fuse/inode.c`).
        rdev: metadata.rdev.encode(),
        blksize: snapshot.superblock().sectorsize(),
        flags: 0,
    }
}
