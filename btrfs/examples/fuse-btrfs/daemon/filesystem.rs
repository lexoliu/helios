//! The kernel-facing half: FUSE requests onto [`State`].
//!
//! fuser hands requests to a `Sync` filesystem by shared reference, so the
//! state sits behind a mutex. The session runs one event-loop thread, so the
//! lock is never contended; it holds the writer's exclusive ownership.
//!
//! Operations the writer has no API for answer the errno of a filesystem that
//! lacks the feature, and nothing else:
//!
//! - `setattr` changing the file flags: `EOPNOTSUPP`
//! - `setxattr`, `removexattr`, `fallocate`: `EOPNOTSUPP`
//! - `rename` with `RENAME_WHITEOUT`, or with both `RENAME_NOREPLACE` and
//!   `RENAME_EXCHANGE`: `EINVAL`
//!
//! Left to fuser's default of `ENOSYS`: `access`, `bmap`, `ioctl`, `poll`,
//! `getlk`, `setlk`, `lseek` and `copy_file_range`.

use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuser::{
    BsdFileFlags, Errno, FileHandle, Filesystem, FopenFlags, INodeNo, LockOwner, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyStatfs,
    ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use futures_lite::future::block_on;
use helios_btrfs::write::Special;
use helios_btrfs::{DeviceNumber, Timestamp};
use helios_hal::fs::RenameMode;

use super::state::{Node, Owner, Setattr, State, target_bytes, timestamp};

/// Attributes and entries are never cached: every change is made through this
/// daemon, and the kernel revalidating on each use keeps a reply the same as
/// what the next request sees.
const TTL: Duration = Duration::ZERO;

/// The longest name `statfs` reports, `BTRFS_NAME_LEN`.
const NAME_MAX: u32 = 255;

pub struct Daemon {
    state: Mutex<State>,
}

impl Daemon {
    pub fn new(state: State) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    fn run<R>(&self, request: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .expect("an earlier request panicked while holding the filesystem");
        request(&mut state)
    }
}

fn now() -> Timestamp {
    timestamp(SystemTime::now())
}

/// A new node's permission bits: what the caller asked for, less its umask.
fn permissions(mode: u32, umask: u32) -> u32 {
    mode & !umask & 0o7777
}

impl Filesystem for Daemon {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.run(|state| block_on(state.lookup(parent, name))) {
            Ok((attr, generation)) => reply.entry(&TTL, &attr, generation),
            Err(errno) => reply.error(errno),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.run(|state| state.forget(ino, nlookup));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.run(|state| block_on(state.getattr(ino))) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if flags.is_some() {
            return reply.error(Errno::EOPNOTSUPP);
        }
        let change = Setattr {
            mode,
            uid,
            gid,
            size,
            access: atime,
            modified: mtime,
        };
        match self.run(|state| block_on(state.setattr(ino, change, now()))) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.run(|state| block_on(state.readlink(ino))) {
            Ok(target) => reply.data(&target),
            Err(errno) => reply.error(errno),
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        // The kernel sends `mknod`'s own encoding of the number
        // (`fuse_mknod` in `fs/fuse/dir.c` applies `new_encode_dev`).
        let device = DeviceNumber::decode(rdev);
        let node = match mode & libc::S_IFMT {
            libc::S_IFREG => Node::File,
            libc::S_IFCHR => Node::Special(Special::CharDevice(device)),
            libc::S_IFBLK => Node::Special(Special::BlockDevice(device)),
            libc::S_IFIFO => Node::Special(Special::Fifo),
            libc::S_IFSOCK => Node::Special(Special::Socket),
            // `vfs_mknod` refuses every other type before it reaches a
            // filesystem.
            _ => return reply.error(Errno::EINVAL),
        };
        self.create_entry(req, parent, name, node, permissions(mode, umask), reply);
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        self.create_entry(
            req,
            parent,
            name,
            Node::Directory,
            permissions(mode, umask),
            reply,
        );
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        self.create_entry(
            req,
            parent,
            link_name,
            Node::Symlink(target_bytes(target)),
            0o777,
            reply,
        );
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let owner = Owner {
            uid: req.uid(),
            gid: req.gid(),
        };
        let permissions = permissions(mode, umask);
        match self.run(|state| {
            block_on(state.create(parent, name, Node::File, permissions, owner, now()))
        }) {
            Ok((attr, generation)) => {
                reply.created(&TTL, &attr, generation, FileHandle(0), FopenFlags::empty())
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match self.run(|state| block_on(state.link(ino, newparent, newname, now()))) {
            Ok((attr, generation)) => reply.entry(&TTL, &attr, generation),
            Err(errno) => reply.error(errno),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        finish(
            self.run(|state| block_on(state.remove(parent, name, false, now()))),
            reply,
        );
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        finish(
            self.run(|state| block_on(state.remove(parent, name, true, now()))),
            reply,
        );
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        // Both flags together, `RENAME_WHITEOUT` (a whiteout is a device
        // node, which the writer cannot create) and unknown bits are `EINVAL`
        // as on Linux.
        let mode = if flags.is_empty() {
            RenameMode::Replace
        } else if flags == RenameFlags::RENAME_NOREPLACE {
            RenameMode::NoReplace
        } else if flags == RenameFlags::RENAME_EXCHANGE {
            RenameMode::Exchange
        } else {
            return reply.error(Errno::EINVAL);
        };
        finish(
            self.run(|state| block_on(state.rename(parent, name, newparent, newname, mode, now()))),
            reply,
        );
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.run(|state| block_on(state.read(ino, offset, size))) {
            Ok(data) => reply.data(&data),
            Err(errno) => reply.error(errno),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.run(|state| block_on(state.write(ino, offset, data, now()))) {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
        }
    }

    // Closing a file makes nothing durable on Linux's btrfs either: every
    // reply's change is already visible, and `fsync` is what commits it.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // The writer's commit is the whole filesystem's, so `fdatasync` commits
    // what `fsync` does.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        finish(self.run(|state| block_on(state.sync())), reply);
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        finish(self.run(|state| block_on(state.sync())), reply);
    }

    /// The unmount: what the running transaction holds becomes durable
    /// before the daemon lets the device go.
    fn destroy(&mut self) {
        let state = self
            .state
            .get_mut()
            .expect("an earlier request panicked while holding the filesystem");
        if let Err(errno) = block_on(state.sync()) {
            tracing::error!(
                ?errno,
                "the unmount commit failed; the device keeps the last one"
            );
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let listed = self.run(|state| {
            block_on(state.readdir(ino, offset, |entry, cookie, kind, name| {
                reply.add(entry, cookie, kind, name)
            }))
        });
        match listed {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.run(|state| state.statfs()) {
            Ok(statfs) => reply.statfs(
                statfs.blocks,
                statfs.free,
                statfs.free,
                // The format keeps no inode count, as Linux's btrfs reports.
                0,
                0,
                statfs.block_size,
                NAME_MAX,
                statfs.block_size,
            ),
            Err(errno) => reply.error(errno),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.run(|state| block_on(state.getxattr(ino, name, size))) {
            Ok((length, _)) if size == 0 => reply.size(length as u32),
            Ok((length, _)) if length > size as usize => reply.error(Errno::ERANGE),
            Ok((_, value)) => reply.data(&value),
            Err(errno) => reply.error(errno),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.run(|state| block_on(state.listxattr(ino))) {
            Ok(names) if size == 0 => reply.size(names.len() as u32),
            Ok(names) if names.len() > size as usize => reply.error(Errno::ERANGE),
            Ok(names) => reply.data(&names),
            Err(errno) => reply.error(errno),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EOPNOTSUPP);
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _length: u64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }
}

impl Daemon {
    fn create_entry(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        node: Node<'_>,
        permissions: u32,
        reply: ReplyEntry,
    ) {
        let owner = Owner {
            uid: req.uid(),
            gid: req.gid(),
        };
        match self
            .run(|state| block_on(state.create(parent, name, node, permissions, owner, now())))
        {
            Ok((attr, generation)) => reply.entry(&TTL, &attr, generation),
            Err(errno) => reply.error(errno),
        }
    }
}

fn finish(result: Result<(), Errno>, reply: ReplyEmpty) {
    match result {
        Ok(()) => reply.ok(),
        Err(errno) => reply.error(errno),
    }
}
