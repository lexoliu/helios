//! What a node's Unix metadata means, which the mount decides.
//!
//! Storage keeps ownership, modes with the set-id and sticky bits, and
//! device nodes the way Linux keeps them on that filesystem. None of it
//! grants or denies a component program anything: the rights of the mount
//! the program reached the filesystem through are all it holds, and no
//! mount or component interface carries a user or a group (AGENTS.md §3.8).
//!
//! A mount is granted as [`MountSharing::Private`] or
//! [`MountSharing::Shared`], and that never changes while it exists:
//!
//! - **Private.** The mount belongs to one container, and within the
//!   mount's rights the kernel enforces the Unix permissions the
//!   filesystem stores, as Linux does. A container's processes see the
//!   stored metadata.
//! - **Shared.** The mount may be granted to several containers and to
//!   component programs at once. In each container it behaves as it does
//!   for a component program: every node appears owned by the identity the container's
//!   configuration names, its modes follow the mount's rights, and the
//!   mount is nosuid and nodev, with file capabilities ignored
//!   ([`MountGrant::container_view`], [`MountGrant::flags`]).
//!
//! A node created by a caller with no Unix identity (a non-Linux program,
//! or any program on a shared mount) takes its owner and group from its
//! parent directory, and its mode from the parent's with the execute bits
//! cleared for a regular file ([`UnixAttributes::inherited`]).
//!
//! Concurrency contract: every type here is a plain value, computed on the
//! caller's processor; nothing is shared or locked.

use bitflags::bitflags;
use helios_hal::fs::{FileSystemRights, NodeKind};

/// The set-group-id bit, from `include/uapi/linux/stat.h`.
const S_ISGID: u32 = 0o2000;
/// Read, write and execute for owner, group and others (`S_IRWXUGO`).
const S_IRWXUGO: u32 = 0o777;
/// Read for owner, group and others (`S_IRUGO`).
const S_IRUGO: u32 = 0o444;
/// Write for owner, group and others (`S_IWUGO`).
const S_IWUGO: u32 = 0o222;
/// Execute, or search on a directory, for owner, group and others
/// (`S_IXUGO`).
const S_IXUGO: u32 = 0o111;

/// Whether a mount belongs to one container or may be seen by many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountSharing {
    /// Belongs to one container, which sees the Unix metadata the
    /// filesystem stores and is held to it.
    Private,
    /// May be granted to several containers and to component programs at
    /// once; every container sees it as a component program does.
    Shared,
}

/// What a mount grants: the rights every program holds through it, and
/// whether it is private or shared. Both are fixed for the mount's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MountGrant {
    pub rights: FileSystemRights,
    pub sharing: MountSharing,
}

bitflags! {
    /// The mount flags `statfs` reports in `f_flags`, from
    /// `include/linux/statfs.h`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MountFlags: u64 {
        const READ_ONLY = 0x0001;
        /// Set-id bits and file capabilities confer nothing at `execve`.
        const NO_SUID = 0x0002;
        /// Device nodes are never opened.
        const NO_DEV = 0x0004;
    }
}

impl MountFlags {
    /// Whether `execve` honours a file's set-id bits and its
    /// `security.capability`. Linux skips both on a nosuid mount:
    /// `bprm_fill_uid` in `fs/exec.c` and `get_file_caps` in
    /// `security/commoncap.c` each return early when `mnt_may_suid` fails.
    pub const fn honours_exec_privileges(self) -> bool {
        !self.contains(Self::NO_SUID)
    }

    /// Whether a device node on the mount may be opened, as `may_open_dev`
    /// in `fs/namei.c` asks of `MNT_NODEV`.
    pub const fn opens_devices(self) -> bool {
        !self.contains(Self::NO_DEV)
    }
}

/// The ownership and permission bits a Unix filesystem stores on a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnixAttributes {
    /// The permission, set-id and sticky bits (`07777`), without the file
    /// type.
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
}

impl UnixAttributes {
    /// The attributes a node of `kind` takes when a caller with no Unix
    /// identity creates it in a directory that has `self`.
    ///
    /// The owner and group are the directory's. The mode is the
    /// directory's permission bits; a regular file, and any other kind
    /// that holds no directory entries, loses the execute bits. A
    /// directory keeps the parent's set-id group bit, which Linux gives a
    /// directory made in a set-group-id directory (`inode_init_owner` in
    /// `fs/inode.c`); nothing else inherits a set-id or sticky bit. A
    /// symbolic link is `0777`, the only mode Linux gives one
    /// (`vfs_symlink` in `fs/namei.c` creates it `S_IALLUGO`, and btrfs
    /// stores `S_IFLNK | S_IRWXUGO`).
    pub const fn inherited(self, kind: NodeKind) -> Self {
        let permissions = match kind {
            NodeKind::Directory => (self.permissions & S_IRWXUGO) | (self.permissions & S_ISGID),
            NodeKind::Symlink => S_IRWXUGO,
            NodeKind::File
            | NodeKind::CharacterDevice
            | NodeKind::BlockDevice
            | NodeKind::Fifo
            | NodeKind::Socket => self.permissions & S_IRWXUGO & !S_IXUGO,
        };
        Self {
            permissions,
            uid: self.uid,
            gid: self.gid,
        }
    }
}

/// The owner and group a container's configuration names for every node
/// of a shared mount it is granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedOwner {
    pub uid: u32,
    pub gid: u32,
}

impl MountGrant {
    /// The mount flags a container sees on this mount: read-only without
    /// the write right, and nosuid and nodev when it is shared.
    pub fn flags(self) -> MountFlags {
        let mut flags = MountFlags::empty();
        if !self.rights.contains(FileSystemRights::WRITE) {
            flags |= MountFlags::READ_ONLY;
        }
        if self.sharing == MountSharing::Shared {
            flags |= MountFlags::NO_SUID | MountFlags::NO_DEV;
        }
        flags
    }

    /// What a container's processes see of a node of `kind` whose stored
    /// attributes are `stored`.
    ///
    /// A private mount shows what the filesystem stores. A shared mount
    /// shows every node owned by `owner`, the identity the container's
    /// configuration names, with modes that follow the mount's rights:
    /// read for every class with the read right, write with the write
    /// right, and execute (search, on a directory) with the read right,
    /// since a mount grants no execute right of its own and what it lets
    /// a program read it lets the program run. No set-id or sticky bit
    /// shows. A symbolic link is `0777`, and a node that holds no data and
    /// no entries (a device node, a FIFO, a socket) has no execute bits.
    pub fn container_view(
        self,
        kind: NodeKind,
        stored: UnixAttributes,
        owner: SharedOwner,
    ) -> UnixAttributes {
        if self.sharing == MountSharing::Private {
            return stored;
        }
        let readable = self.rights.contains(FileSystemRights::READ);
        let writable = self.rights.contains(FileSystemRights::WRITE);
        let mut granted = 0;
        if readable {
            granted |= S_IRUGO;
        }
        if writable {
            granted |= S_IWUGO;
        }
        let permissions = match kind {
            NodeKind::Symlink => S_IRWXUGO,
            NodeKind::Directory | NodeKind::File if readable => granted | S_IXUGO,
            NodeKind::Directory
            | NodeKind::File
            | NodeKind::CharacterDevice
            | NodeKind::BlockDevice
            | NodeKind::Fifo
            | NodeKind::Socket => granted,
        };
        UnixAttributes {
            permissions,
            uid: owner.uid,
            gid: owner.gid,
        }
    }

    /// The `security.capability` value `execve` considers on this mount,
    /// from the attribute `stored` on the file. A shared mount is nosuid,
    /// so `get_file_caps` in `security/commoncap.c` ignores any stored
    /// capability (`!mnt_may_suid`).
    pub fn file_capability<'a>(self, stored: Option<&'a [u8]>) -> Option<&'a [u8]> {
        if self.flags().honours_exec_privileges() {
            stored
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_WRITE: FileSystemRights = FileSystemRights::READ.union(FileSystemRights::WRITE);
    const KINDS: [NodeKind; 7] = [
        NodeKind::Directory,
        NodeKind::File,
        NodeKind::Symlink,
        NodeKind::CharacterDevice,
        NodeKind::BlockDevice,
        NodeKind::Fifo,
        NodeKind::Socket,
    ];

    fn grant(rights: FileSystemRights, sharing: MountSharing) -> MountGrant {
        MountGrant { rights, sharing }
    }

    /// A setuid root binary, a set-group-id directory and a device node,
    /// as a Linux image stores them.
    fn stored(permissions: u32) -> UnixAttributes {
        UnixAttributes {
            permissions,
            uid: 0,
            gid: 0,
        }
    }

    const CONTAINER: SharedOwner = SharedOwner {
        uid: 1000,
        gid: 1000,
    };

    #[test]
    fn a_created_node_inherits_owner_group_and_mode_from_its_parent() {
        let parent = UnixAttributes {
            permissions: 0o2775,
            uid: 1000,
            gid: 100,
        };
        let created = |kind| parent.inherited(kind);
        assert_eq!(
            created(NodeKind::File),
            UnixAttributes {
                permissions: 0o664,
                uid: 1000,
                gid: 100,
            }
        );
        assert_eq!(created(NodeKind::Directory).permissions, 0o2775);
        assert_eq!(created(NodeKind::Symlink).permissions, 0o777);
        for kind in KINDS {
            let node = created(kind);
            assert_eq!((node.uid, node.gid), (1000, 100), "{kind:?}");
        }
        // /tmp: the sticky bit never passes to what is made in it.
        let tmp = UnixAttributes {
            permissions: 0o1777,
            uid: 0,
            gid: 0,
        };
        assert_eq!(tmp.inherited(NodeKind::File).permissions, 0o666);
        assert_eq!(tmp.inherited(NodeKind::Directory).permissions, 0o777);
        // A set-user-id directory passes nothing of it on.
        let odd = UnixAttributes {
            permissions: 0o4750,
            uid: 7,
            gid: 8,
        };
        assert_eq!(odd.inherited(NodeKind::File).permissions, 0o640);
        assert_eq!(odd.inherited(NodeKind::Directory).permissions, 0o750);
        assert_eq!(odd.inherited(NodeKind::Fifo).permissions, 0o640);
    }

    #[test]
    fn a_private_mount_shows_what_the_filesystem_stores() {
        for rights in [READ_WRITE, FileSystemRights::READ] {
            let private = grant(rights, MountSharing::Private);
            for permissions in [0o4755, 0o2775, 0o1777, 0o660] {
                for kind in KINDS {
                    assert_eq!(
                        private.container_view(kind, stored(permissions), CONTAINER),
                        stored(permissions)
                    );
                }
            }
            let flags = private.flags();
            assert!(flags.honours_exec_privileges());
            assert!(flags.opens_devices());
            assert_eq!(
                flags.contains(MountFlags::READ_ONLY),
                rights == FileSystemRights::READ
            );
        }
    }

    #[test]
    fn a_shared_mount_shows_the_container_owner_and_the_mount_rights() {
        let shared = grant(READ_WRITE, MountSharing::Shared);
        let read_only = grant(FileSystemRights::READ, MountSharing::Shared);
        for permissions in [0o4755, 0o6711, 0o2775, 0o1777, 0o000, 0o600] {
            for kind in KINDS {
                let view = shared.container_view(kind, stored(permissions), CONTAINER);
                // Owner: the container's identity, whatever is stored.
                assert_eq!((view.uid, view.gid), (1000, 1000), "{kind:?}");
                // No set-id or sticky bit shows.
                assert_eq!(view.permissions & 0o7000, 0, "{kind:?} {permissions:o}");
            }
            let view = |grant: MountGrant, kind| {
                grant
                    .container_view(kind, stored(permissions), CONTAINER)
                    .permissions
            };
            // Modes: the mount's rights, never the stored bits.
            assert_eq!(view(shared, NodeKind::File), 0o777);
            assert_eq!(view(shared, NodeKind::Directory), 0o777);
            assert_eq!(view(read_only, NodeKind::File), 0o555);
            assert_eq!(view(read_only, NodeKind::Directory), 0o555);
            assert_eq!(view(shared, NodeKind::Symlink), 0o777);
            assert_eq!(view(read_only, NodeKind::Symlink), 0o777);
            for kind in [
                NodeKind::CharacterDevice,
                NodeKind::BlockDevice,
                NodeKind::Fifo,
                NodeKind::Socket,
            ] {
                assert_eq!(view(shared, kind), 0o666, "{kind:?}");
                assert_eq!(view(read_only, kind), 0o444, "{kind:?}");
            }
        }
    }

    #[test]
    fn a_shared_mount_is_nosuid_and_nodev_and_ignores_file_capabilities() {
        const VFS_CAP_V2: &[u8] = &[
            0x01, 0x00, 0x00, 0x02, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        for rights in [READ_WRITE, FileSystemRights::READ] {
            let shared = grant(rights, MountSharing::Shared);
            let flags = shared.flags();
            assert!(flags.contains(MountFlags::NO_SUID | MountFlags::NO_DEV));
            // nosuid is what makes `execve` ignore both the set-id bits
            // and `security.capability`.
            assert!(!flags.honours_exec_privileges());
            assert!(!flags.opens_devices());
            assert_eq!(shared.file_capability(Some(VFS_CAP_V2)), None);
            assert_eq!(shared.file_capability(None), None);
            assert_eq!(
                grant(rights, MountSharing::Private).file_capability(Some(VFS_CAP_V2)),
                Some(VFS_CAP_V2)
            );
            assert_eq!(
                flags.contains(MountFlags::READ_ONLY),
                rights == FileSystemRights::READ
            );
        }
        assert_eq!(
            grant(READ_WRITE, MountSharing::Shared).flags().bits(),
            0x0006
        );
    }
}
