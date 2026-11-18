//! POSIX ACLs, kept as Linux's btrfs keeps them: an `XATTR_ITEM` named
//! `system.posix_acl_access` or `system.posix_acl_default` holding the uapi
//! encoding `posix_acl_to_xattr` produces (`include/uapi/linux/posix_acl_xattr.h`:
//! a little-endian version word, then one `{tag, perm, id}` entry per ACL
//! entry), written by `__btrfs_set_acl` in `fs/btrfs/acl.c`.
//!
//! Linux reaches the storage through the VFS (`do_set_acl`, `vfs_set_acl`,
//! `set_posix_acl` and `vfs_remove_acl` in `fs/posix_acl.c`), which parses
//! and validates the value and refuses an ACL on a symlink or a default ACL
//! on a non-directory. `btrfs_set_acl` then takes the permission bits of an
//! access ACL into the inode's mode and stores nothing for an access ACL the
//! mode represents (`posix_acl_update_mode`), `btrfs_setattr` carries a mode
//! change into the access ACL (`posix_acl_chmod`), and
//! `btrfs_new_inode_prepare` gives a new inode the ACLs its directory's
//! default ACL implies (`posix_acl_create`). This module does each of those
//! as Linux does.
//!
//! `posix_acl_update_mode` also clears S_ISGID when the caller is neither in
//! the file's group nor holds `CAP_FSETID`. The storage layer has no caller;
//! that clearing is the caller's (lexoliu/helios#1084).

use alloc::vec::Vec;

use super::Transaction;
use super::xattr::XattrMode;
use crate::source::BlockSink;
use crate::tree::write::WriteError;
use crate::{FileKind, Timestamp};

/// `XATTR_NAME_POSIX_ACL_ACCESS` and `XATTR_NAME_POSIX_ACL_DEFAULT`
/// (`include/uapi/linux/xattr.h`).
pub(crate) const ACCESS_NAME: &[u8] = b"system.posix_acl_access";
pub(crate) const DEFAULT_NAME: &[u8] = b"system.posix_acl_default";

/// `POSIX_ACL_XATTR_VERSION` (`include/uapi/linux/posix_acl_xattr.h`).
const VERSION: u32 = 2;
/// `ACL_UNDEFINED_ID`: the id of an entry that names no user or group.
const UNDEFINED_ID: u32 = u32::MAX;
/// `struct posix_acl_xattr_header` and `struct posix_acl_xattr_entry`.
const HEADER_BYTES: usize = 4;
const ENTRY_BYTES: usize = 8;

/// Entry tags and permission bits, `include/uapi/linux/posix_acl.h`.
const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;
const PERMISSIONS: u16 = 0o7;

/// `S_IRWXUGO`, the bits an ACL's base entries mirror.
const S_IRWXUGO: u32 = 0o777;

/// Which of an inode's two ACLs: `ACL_TYPE_ACCESS` or `ACL_TYPE_DEFAULT`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AclKind {
    Access,
    Default,
}

impl AclKind {
    /// `posix_acl_type`: the ACL a name stands for.
    pub(crate) fn of(name: &[u8]) -> Option<Self> {
        match name {
            ACCESS_NAME => Some(Self::Access),
            DEFAULT_NAME => Some(Self::Default),
            _ => None,
        }
    }

    fn name(self) -> &'static [u8] {
        match self {
            Self::Access => ACCESS_NAME,
            Self::Default => DEFAULT_NAME,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    tag: u16,
    perm: u16,
    /// The uid or gid of a `USER` or `GROUP` entry.
    id: u32,
}

/// A parsed ACL, `struct posix_acl`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Acl(Vec<Entry>);

impl Acl {
    /// `posix_acl_from_xattr`: `None` for a header with no entries.
    fn parse(value: &[u8]) -> Result<Option<Self>, WriteError> {
        // `posix_acl_fix_xattr_common` and `posix_acl_xattr_count`.
        if value.len() < HEADER_BYTES {
            return Err(WriteError::AclInvalid);
        }
        let version = u32::from_le_bytes(value[..HEADER_BYTES].try_into().unwrap());
        if version != VERSION {
            return Err(WriteError::AclVersion { version });
        }
        let entries = &value[HEADER_BYTES..];
        if !entries.len().is_multiple_of(ENTRY_BYTES) {
            return Err(WriteError::AclInvalid);
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let mut parsed = Vec::with_capacity(entries.len() / ENTRY_BYTES);
        for entry in entries.as_chunks::<ENTRY_BYTES>().0 {
            let tag = u16::from_le_bytes([entry[0], entry[1]]);
            let perm = u16::from_le_bytes([entry[2], entry[3]]);
            let id = u32::from_le_bytes(entry[4..].try_into().unwrap());
            let id = match tag {
                USER_OBJ | GROUP_OBJ | MASK | OTHER => UNDEFINED_ID,
                // `make_kuid`/`make_kgid` in the initial namespace map every
                // id but -1, which `uid_valid` refuses.
                USER | GROUP if id != UNDEFINED_ID => id,
                _ => return Err(WriteError::AclInvalid),
            };
            parsed.push(Entry { tag, perm, id });
        }
        Ok(Some(Self(parsed)))
    }

    /// `posix_acl_valid`: base entries in order, named entries needing a
    /// mask, and only the three permission bits.
    fn validate(&self) -> Result<(), WriteError> {
        #[derive(PartialEq)]
        enum State {
            UserObj,
            User,
            Group,
            Other,
            Done,
        }
        let mut state = State::UserObj;
        let mut needs_mask = false;
        for entry in &self.0 {
            if entry.perm & !PERMISSIONS != 0 {
                return Err(WriteError::AclInvalid);
            }
            state = match (entry.tag, state) {
                (USER_OBJ, State::UserObj) => State::User,
                (USER, State::User) => {
                    needs_mask = true;
                    State::User
                }
                (GROUP_OBJ, State::User) => State::Group,
                (GROUP, State::Group) => {
                    needs_mask = true;
                    State::Group
                }
                (MASK, State::Group) => State::Other,
                (OTHER, State::Other) => State::Done,
                (OTHER, State::Group) if !needs_mask => State::Done,
                _ => return Err(WriteError::AclInvalid),
            };
        }
        if state == State::Done {
            Ok(())
        } else {
            Err(WriteError::AclInvalid)
        }
    }

    /// `posix_acl_to_xattr`.
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES + self.0.len() * ENTRY_BYTES);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        for entry in &self.0 {
            bytes.extend_from_slice(&entry.tag.to_le_bytes());
            bytes.extend_from_slice(&entry.perm.to_le_bytes());
            bytes.extend_from_slice(&entry.id.to_le_bytes());
        }
        bytes
    }

    /// `posix_acl_equiv_mode`: `mode` with its permission bits taken from the
    /// ACL, and whether the ACL holds more than those bits say.
    fn equivalent_mode(&self, mode: u32) -> (u32, bool) {
        let mut bits = 0;
        let mut beyond = false;
        for entry in &self.0 {
            let perm = u32::from(entry.perm & PERMISSIONS);
            match entry.tag {
                USER_OBJ => bits |= perm << 6,
                GROUP_OBJ => bits |= perm << 3,
                OTHER => bits |= perm,
                MASK => {
                    bits = (bits & !0o070) | (perm << 3);
                    beyond = true;
                }
                _ => beyond = true,
            }
        }
        ((mode & !S_IRWXUGO) | bits, beyond)
    }

    /// `posix_acl_create_masq`: narrows a copy of a directory's default ACL
    /// by the mode a creation asks for, and the mode by the ACL. Returns
    /// whether the ACL holds more than the mode says.
    fn create_masq(&mut self, mode: &mut u32) -> Result<bool, WriteError> {
        let mut bits = *mode;
        let mut beyond = false;
        let (mut group, mut mask) = (None, None);
        for (index, entry) in self.0.iter_mut().enumerate() {
            match entry.tag {
                USER_OBJ => {
                    entry.perm &= (bits >> 6) as u16 | !PERMISSIONS;
                    bits &= (u32::from(entry.perm) << 6) | !0o700;
                }
                USER | GROUP => beyond = true,
                GROUP_OBJ => group = Some(index),
                OTHER => {
                    entry.perm &= bits as u16 | !PERMISSIONS;
                    bits &= u32::from(entry.perm) | !0o007;
                }
                MASK => {
                    mask = Some(index);
                    beyond = true;
                }
                _ => return Err(WriteError::AclInvalid),
            }
        }
        let governing = mask.or(group).ok_or(WriteError::AclInvalid)?;
        let entry = &mut self.0[governing];
        entry.perm &= (bits >> 3) as u16 | !PERMISSIONS;
        bits &= (u32::from(entry.perm) << 3) | !0o070;
        *mode = (*mode & !S_IRWXUGO) | bits;
        Ok(beyond)
    }

    /// `__posix_acl_chmod_masq`: the base entries, or the mask in place of
    /// the owning group, take `mode`'s permission bits.
    fn chmod_masq(&mut self, mode: u32) -> Result<(), WriteError> {
        let (mut group, mut mask) = (None, None);
        for (index, entry) in self.0.iter_mut().enumerate() {
            match entry.tag {
                USER_OBJ => entry.perm = ((mode & 0o700) >> 6) as u16,
                USER | GROUP => {}
                GROUP_OBJ => group = Some(index),
                MASK => mask = Some(index),
                OTHER => entry.perm = (mode & 0o007) as u16,
                _ => return Err(WriteError::AclInvalid),
            }
        }
        let governing = mask.or(group).ok_or(WriteError::AclInvalid)?;
        self.0[governing].perm = ((mode & 0o070) >> 3) as u16;
        Ok(())
    }
}

/// What `posix_acl_create` gives a new inode besides its narrowed mode.
#[derive(Default)]
pub(crate) struct Inherited {
    default: Option<Acl>,
    access: Option<Acl>,
}

impl<S: BlockSink> Transaction<S> {
    /// The stored ACL of `kind`, parsed as `btrfs_get_acl` parses it.
    async fn stored_acl(
        &self,
        tree: u64,
        inode: u64,
        kind: AclKind,
    ) -> Result<Option<Acl>, WriteError> {
        match self.stored_xattr(tree, inode, kind.name()).await? {
            Some(value) => Acl::parse(&value),
            None => Ok(None),
        }
    }

    /// `setxattr(2)` of an ACL name: `do_set_acl`, `vfs_set_acl` and
    /// `set_posix_acl`. An empty value, or a header with no entries, removes
    /// the ACL. The `XATTR_CREATE` and `XATTR_REPLACE` flags do not apply.
    pub(crate) async fn set_acl(
        &mut self,
        tree: u64,
        inode: u64,
        kind: AclKind,
        value: &[u8],
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let acl = if value.is_empty() {
            None
        } else {
            Acl::parse(value)?
        };
        let kind_of_file = self.check_acl_write(tree, inode).await?;
        if kind == AclKind::Default && kind_of_file != FileKind::Directory {
            return match acl {
                Some(_) => Err(WriteError::AclDefaultNotDirectory { tree, inode }),
                None => Ok(()),
            };
        }
        if let Some(acl) = &acl {
            acl.validate()?;
        }
        self.store_acl(tree, inode, kind, acl, when).await
    }

    /// `removexattr(2)` of an ACL name: `vfs_remove_acl`, which removes an
    /// absent ACL without error, and a default ACL of a non-directory without
    /// touching the inode.
    pub(crate) async fn remove_acl(
        &mut self,
        tree: u64,
        inode: u64,
        kind: AclKind,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let kind_of_file = self.check_acl_write(tree, inode).await?;
        if kind == AclKind::Default && kind_of_file != FileKind::Directory {
            return Ok(());
        }
        self.store_acl(tree, inode, kind, None, when).await
    }

    /// `may_write_xattr`, then `set_posix_acl`'s refusal of a file with no
    /// `set_acl` operation: a btrfs symlink (`btrfs_symlink_inode_operations`
    /// in `fs/btrfs/inode.c`).
    async fn check_acl_write(&self, tree: u64, inode: u64) -> Result<FileKind, WriteError> {
        let kind = self.check_xattr_writable(tree, inode).await?;
        if kind == FileKind::Symlink {
            return Err(WriteError::AclNotSupported { tree, inode });
        }
        Ok(kind)
    }

    /// `vfs_get_acl`'s refusal of a symlink, before the stored value is read.
    pub(crate) async fn check_acl_read(&self, tree: u64, inode: u64) -> Result<(), WriteError> {
        let (_, metadata) = self.inode_item(tree, inode).await?;
        if metadata.kind == FileKind::Symlink {
            return Err(WriteError::AclNotSupported { tree, inode });
        }
        Ok(())
    }

    /// `btrfs_set_acl`, then `__btrfs_set_acl` through
    /// `btrfs_setxattr_trans`: an access ACL sets the mode's permission bits
    /// and is not stored when the mode says all it does; the item is written
    /// or removed, and the inode changes.
    async fn store_acl(
        &mut self,
        tree: u64,
        inode: u64,
        kind: AclKind,
        mut acl: Option<Acl>,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let mut mode = None;
        if let (AclKind::Access, Some(access)) = (kind, &acl) {
            let (raw, _) = self.inode_item(tree, inode).await?;
            let (updated, beyond) = access.equivalent_mode(raw.mode.get());
            mode = Some(updated);
            if !beyond {
                acl = None;
            }
        }
        match acl {
            Some(acl) => {
                self.write_xattr(tree, inode, kind.name(), &acl.encode(), XattrMode::Set)
                    .await?;
            }
            None => {
                self.drop_xattr(tree, inode, kind.name()).await?;
            }
        }
        self.touch(tree, inode, when, mode).await
    }

    /// `posix_acl_chmod`, which `btrfs_setattr` runs after a mode change: a
    /// stored access ACL takes the new permission bits and is set again.
    pub(crate) async fn chmod_acl(
        &mut self,
        tree: u64,
        inode: u64,
        mode: u32,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let Some(mut acl) = self.stored_acl(tree, inode, AclKind::Access).await? else {
            return Ok(());
        };
        acl.chmod_masq(mode)?;
        self.store_acl(tree, inode, AclKind::Access, Some(acl), when)
            .await
    }

    /// Whether `directory` has a default ACL, which a creation in it
    /// inherits: two more items to reserve (`btrfs_new_inode_prepare`).
    pub(crate) async fn has_default_acl(
        &self,
        tree: u64,
        directory: u64,
    ) -> Result<bool, WriteError> {
        Ok(self
            .stored_xattr(tree, directory, DEFAULT_NAME)
            .await?
            .is_some())
    }

    /// `posix_acl_create`: a new inode's mode in `directory`, narrowed by the
    /// directory's default ACL when it has one and by `umask` when it has
    /// none, and the ACLs the inode inherits. A symlink inherits nothing and
    /// keeps its mode.
    pub(crate) async fn inherit_acls(
        &self,
        tree: u64,
        directory: u64,
        mode: &mut u32,
        umask: u32,
    ) -> Result<Inherited, WriteError> {
        if *mode & super::ownership::S_IFMT == super::ownership::S_IFLNK {
            return Ok(Inherited::default());
        }
        let Some(default) = self.stored_acl(tree, directory, AclKind::Default).await? else {
            *mode &= !umask;
            return Ok(Inherited::default());
        };
        let mut access = default.clone();
        let beyond = access.create_masq(mode)?;
        Ok(Inherited {
            access: beyond.then_some(access),
            default: (*mode & super::ownership::S_IFMT == super::ownership::S_IFDIR)
                .then_some(default),
        })
    }

    /// `btrfs_init_inode_security`: the inherited default ACL, then the
    /// access ACL, written in the creating transaction without a further
    /// inode update.
    pub(crate) async fn store_inherited(
        &mut self,
        tree: u64,
        inode: u64,
        inherited: Inherited,
    ) -> Result<(), WriteError> {
        for (kind, acl) in [
            (AclKind::Default, inherited.default),
            (AclKind::Access, inherited.access),
        ] {
            if let Some(acl) = acl {
                self.write_xattr(tree, inode, kind.name(), &acl.encode(), XattrMode::Set)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut bytes = VERSION.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&perm.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes
    }

    const U: u32 = UNDEFINED_ID;

    #[test]
    fn parsing_and_validation_follow_posix_acl_from_xattr_and_posix_acl_valid() {
        assert!(matches!(Acl::parse(&[2, 0]), Err(WriteError::AclInvalid)));
        assert!(matches!(
            Acl::parse(&[1, 0, 0, 0]),
            Err(WriteError::AclVersion { version: 1 })
        ));
        assert_eq!(Acl::parse(&VERSION.to_le_bytes()).unwrap(), None);
        assert!(matches!(
            Acl::parse(&[2, 0, 0, 0, 1, 0]),
            Err(WriteError::AclInvalid)
        ));
        assert!(matches!(
            Acl::parse(&encoded(&[(0x40, 7, U)])),
            Err(WriteError::AclInvalid)
        ));
        assert!(matches!(
            Acl::parse(&encoded(&[(USER, 7, U)])),
            Err(WriteError::AclInvalid)
        ));
        let valid = |entries: &[(u16, u16, u32)]| {
            Acl::parse(&encoded(entries))
                .unwrap()
                .unwrap()
                .validate()
                .is_ok()
        };
        assert!(valid(&[(USER_OBJ, 6, U), (GROUP_OBJ, 4, U), (OTHER, 4, U)]));
        assert!(valid(&[
            (USER_OBJ, 7, U),
            (USER, 6, 1000),
            (GROUP_OBJ, 5, U),
            (GROUP, 4, 1000),
            (MASK, 7, U),
            (OTHER, 0, U),
        ]));
        // A named entry with no mask, base entries out of order, and a bit
        // beyond rwx.
        assert!(!valid(&[
            (USER_OBJ, 7, U),
            (USER, 6, 1000),
            (GROUP_OBJ, 5, U),
            (OTHER, 0, U),
        ]));
        assert!(!valid(&[
            (GROUP_OBJ, 4, U),
            (USER_OBJ, 6, U),
            (OTHER, 4, U)
        ]));
        assert!(!valid(&[
            (USER_OBJ, 8, U),
            (GROUP_OBJ, 4, U),
            (OTHER, 4, U)
        ]));
        assert!(!valid(&[(USER_OBJ, 6, U), (GROUP_OBJ, 4, U)]));
    }

    #[test]
    fn encoding_writes_undefined_ids_for_unnamed_entries() {
        // A base entry's id is ignored on input and written as -1.
        let acl = Acl::parse(&encoded(&[
            (USER_OBJ, 6, 5),
            (USER, 6, 1000),
            (GROUP_OBJ, 4, 9),
            (MASK, 6, 0),
            (OTHER, 4, 7),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            acl.encode(),
            encoded(&[
                (USER_OBJ, 6, U),
                (USER, 6, 1000),
                (GROUP_OBJ, 4, U),
                (MASK, 6, U),
                (OTHER, 4, U),
            ])
        );
    }

    #[test]
    fn modes_follow_equiv_create_and_chmod() {
        let base = Acl::parse(&encoded(&[
            (USER_OBJ, 7, U),
            (GROUP_OBJ, 5, U),
            (OTHER, 1, U),
        ]))
        .unwrap()
        .unwrap();
        // The file type and S_ISGID stay; clearing S_ISGID is the caller's.
        assert_eq!(base.equivalent_mode(0o102_666), (0o102_751, false));
        let named = Acl::parse(&encoded(&[
            (USER_OBJ, 7, U),
            (USER, 7, 1000),
            (GROUP_OBJ, 4, U),
            (MASK, 6, U),
            (OTHER, 0, U),
        ]))
        .unwrap()
        .unwrap();
        // The mask stands for the group bits.
        assert_eq!(named.equivalent_mode(0o040_000), (0o040_760, true));

        // A creation of 0666 under that default ACL: the mask and the
        // owner narrow to rw, and no umask applies.
        let mut access = named.clone();
        let mut mode = 0o100_666;
        assert!(access.create_masq(&mut mode).unwrap());
        assert_eq!(mode, 0o100_660);
        assert_eq!(
            access.encode(),
            encoded(&[
                (USER_OBJ, 6, U),
                (USER, 7, 1000),
                (GROUP_OBJ, 4, U),
                (MASK, 6, U),
                (OTHER, 0, U),
            ])
        );

        // chmod 0751 sets the owner, the mask and the others.
        let mut chmodded = named;
        chmodded.chmod_masq(0o751).unwrap();
        assert_eq!(
            chmodded.encode(),
            encoded(&[
                (USER_OBJ, 7, U),
                (USER, 7, 1000),
                (GROUP_OBJ, 4, U),
                (MASK, 5, U),
                (OTHER, 1, U),
            ])
        );
    }
}
