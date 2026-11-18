//! Changing an inode's permission bits and its owner. Neither operation ever
//! lets a file carry set-id authority: a non-directory refuses S_ISUID and
//! S_ISGID, and a change of owner removes them (lexoliu/helios#1037).

use super::Transaction;
use super::namespace::{check_flags, time};
use crate::items::{INODE_APPEND, INODE_IMMUTABLE};
use crate::source::BlockSink;
use crate::tree::write::WriteError;
use crate::{FileKind, Timestamp};

/// The file type bits of an inode's mode.
pub(crate) const S_IFMT: u32 = 0o170_000;
pub(crate) const S_IFDIR: u32 = 0o040_000;
pub(crate) const S_IFLNK: u32 = 0o120_000;
/// Every bit `chmod` sets: permissions, sticky and the two set-id bits.
pub(crate) const S_IALLUGO: u32 = 0o7777;
pub(crate) const S_ISUID: u32 = 0o4000;
pub(crate) const S_ISGID: u32 = 0o2000;

/// Refuses a mode that gives a non-directory the set-id authority. S_ISGID on
/// a directory makes new entries inherit its group and is legitimate;
/// S_ISUID on a directory has no meaning and is left alone.
pub(crate) fn refuse_set_id(mode: u32) -> Result<(), WriteError> {
    if mode & S_IFMT != S_IFDIR && mode & (S_ISUID | S_ISGID) != 0 {
        return Err(WriteError::SetId {
            permissions: mode & S_IALLUGO,
        });
    }
    Ok(())
}

impl<S: BlockSink> Transaction<S> {
    /// Replaces the permission bits, keeping the file type, as `chmod`
    /// (`chmod_common` in `fs/open.c` stores `(mode & S_IALLUGO) |
    /// (inode->i_mode & ~S_IALLUGO)`), then sets an access ACL's base
    /// entries to them as `btrfs_setattr` does with `posix_acl_chmod`.
    pub(crate) async fn set_permissions(
        &mut self,
        tree: u64,
        inode: u64,
        permissions: u32,
        changed: Timestamp,
    ) -> Result<(), WriteError> {
        if permissions & !S_IALLUGO != 0 {
            return Err(WriteError::InvalidPermissions { permissions });
        }
        let (mut raw, metadata) = self.inode_item(tree, inode).await?;
        check_flags(tree, inode, metadata.flags, INODE_IMMUTABLE | INODE_APPEND)?;
        let mode = (raw.mode.get() & !S_IALLUGO) | permissions;
        refuse_set_id(mode)?;
        let changed_at = changed;
        let changed = time(tree, inode, changed)?;
        raw.mode.set(mode);
        raw.ctime = changed;
        raw.transid.set(self.trees.generation());
        raw.sequence.set(raw.sequence.get().wrapping_add(1));
        self.put_inode(tree, inode, raw).await?;
        // `btrfs_setattr` carries the mode into the access ACL.
        self.chmod_acl(tree, inode, mode, changed_at).await
    }

    /// Sets the owner and/or the group, each left alone when `None` as
    /// `fchownat` leaves an id of -1.
    ///
    /// On a non-directory the set-id bits go, as `chown_common` in
    /// `fs/open.c` of torvalds/linux requests with
    /// `ATTR_KILL_SUID | setattr_should_drop_sgid()`, which `notify_change`
    /// in `fs/attr.c` turns into a mode with `S_ISUID` and `S_ISGID` removed.
    /// Linux drops `S_ISGID` unconditionally when `S_IXGRP` is set and
    /// otherwise only when the caller is neither in the group nor holds
    /// `CAP_FSETID`; the writer has no caller, so the bit goes in both cases,
    /// as it would for an unprivileged caller. A directory keeps both bits.
    ///
    /// `chown_common` asks for `ATTR_KILL_PRIV` on a non-directory as well,
    /// which `notify_change` carries to `security_inode_killpriv` and
    /// `cap_inode_killpriv` (security/commoncap.c): `security.capability`
    /// goes with the old owner ([`Self::kill_privileges`]).
    pub(crate) async fn set_owner(
        &mut self,
        tree: u64,
        inode: u64,
        uid: Option<u32>,
        gid: Option<u32>,
        changed: Timestamp,
    ) -> Result<(), WriteError> {
        let (mut raw, metadata) = self.inode_item(tree, inode).await?;
        check_flags(tree, inode, metadata.flags, INODE_IMMUTABLE | INODE_APPEND)?;
        let changed = time(tree, inode, changed)?;
        self.kill_privileges(tree, inode).await?;
        if let Some(uid) = uid {
            raw.uid.set(uid);
        }
        if let Some(gid) = gid {
            raw.gid.set(gid);
        }
        if metadata.kind != FileKind::Directory {
            raw.mode.set(raw.mode.get() & !(S_ISUID | S_ISGID));
        }
        raw.ctime = changed;
        raw.transid.set(self.trees.generation());
        raw.sequence.set(raw.sequence.get().wrapping_add(1));
        self.put_inode(tree, inode, raw).await
    }
}
