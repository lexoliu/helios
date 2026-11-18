//! Single-owner CoW edits. One transaction owns its node overlay and allocator;
//! reads through a published root never observe this overlay. No lock is held
//! during device I/O. Publication and reclamation belong to the transaction.

use alloc::sync::Arc;
use alloc::vec::Vec;

use arrayvec::ArrayVec;
use rpds::{RedBlackTreeMapSync, VectorSync};
use thiserror::Error;

use super::edit::{Branch, Child, EditError, Header, Leaf};
use super::{Cursor, Expected, Node, NodeSource, TreeRoot};
use crate::ondisk::{MAX_LEVEL, RawHeader, view};
use crate::source::BlockSource;
use crate::volume::Volume;
use crate::{ChecksumType, Error, Key};

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("creation permissions {permissions:#o} include bits outside 07777")]
    InvalidPermissions { permissions: u32 },
    /// A file never carries set-id authority (lexoliu/helios#1037); callers
    /// answer it as not permitted.
    #[error("permissions {permissions:#o} give a non-directory the setuid or setgid bit")]
    SetId { permissions: u32 },
    #[error("inode handle belongs to another mount")]
    ForeignHandle,
    #[error("inode {inode} in tree {tree} no longer has generation {generation}")]
    StaleHandle {
        tree: u64,
        inode: u64,
        generation: u64,
    },
    #[error("inode {inode} in tree {tree} forbids this mutation with flags {flags:#x}")]
    InodeFlags { tree: u64, inode: u64, flags: u64 },
    #[error("read-write mount does not support compat-ro feature {name} ({bit:#x})")]
    UnsupportedCompatRo { name: &'static str, bit: u64 },
    /// Quota groups are not maintained by the writer (lexoliu/helios#1043);
    /// writing would leave Linux's qgroup counts stale.
    #[error(
        "read-write mount does not maintain quota groups, and the root tree holds a quota tree"
    )]
    QuotaTreePresent,
    #[error("subvolume {tree} is read-only")]
    ReadOnlySubvolume { tree: u64 },
    #[error("clone offsets must be sector aligned and any partial sector must end both files")]
    CloneAlignment,
    #[error("clone source range extends beyond the file")]
    CloneSourceRange,
    #[error("clone ranges in one inode overlap")]
    CloneOverlap,
    #[error("clone source and destination have different data-checksum policies")]
    CloneChecksumPolicy,

    #[cfg(feature = "write")]
    #[error(transparent)]
    Encode(#[from] crate::compress::write::EncodeError),
    #[error("compressed writes require data checksums")]
    CompressionWithoutChecksums,

    #[error(
        "directory entry name must be 1..=255 bytes and contain no slash, NUL, dot or dot-dot component"
    )]
    InvalidName,
    #[error("directory {parent} in tree {tree} already contains this name")]
    Exists { tree: u64, parent: u64 },
    #[error("inode or directory index space is exhausted in tree {tree}")]
    IdentifierExhausted { tree: u64 },
    #[error("inode {inode} in tree {tree} cannot have another hard link")]
    LinkLimit { tree: u64, inode: u64 },
    #[error("directory entry is missing from parent {parent} in tree {tree}")]
    NoEntry { tree: u64, parent: u64 },
    #[error("directory {inode} in tree {tree} is not empty")]
    NotEmpty { tree: u64, inode: u64 },
    #[error("namespace item {key} in tree {tree} disagrees with its directory link")]
    NamespaceMismatch { tree: u64, key: Key },
    #[error("subvolume links require a subvolume operation")]
    SubvolumeLink,
    #[error("rename across subvolumes is not permitted")]
    CrossSubvolume,
    #[error("directory move would create a parent cycle")]
    DirectoryCycle,
    #[error("symlink target is empty, contains NUL, or exceeds {maximum} bytes (got {length})")]
    SymlinkTarget { length: usize, maximum: usize },
    #[error("directory hard links are not permitted")]
    DirectoryLink,

    #[error("extended attribute names are 1..=255 bytes (got {length})")]
    XattrNameLength { length: usize },
    #[error("extended attribute name is a namespace prefix with no name after it")]
    XattrName,
    #[error("extended attribute value of {length} bytes exceeds {maximum}")]
    XattrValueTooBig { length: usize, maximum: usize },
    #[error("extended attribute data of {bytes} bytes exceeds the {maximum} one leaf item holds")]
    XattrNoSpace { bytes: usize, maximum: usize },
    #[error("inode {inode} in tree {tree} has no such extended attribute")]
    XattrMissing { tree: u64, inode: u64 },
    #[error("inode {inode} in tree {tree} already has this extended attribute")]
    XattrExists { tree: u64, inode: u64 },
    #[error("extended attributes need {needed} bytes of buffer, got {provided}")]
    XattrBuffer { needed: usize, provided: usize },
    /// Writing `user.*` on a file that is not regular, a directory or a socket.
    #[error("inode {inode} in tree {tree} takes no user.* attributes")]
    XattrNotPermitted { tree: u64, inode: u64 },
    #[error("extended attribute namespace has no handler")]
    XattrNamespace,
    /// A POSIX ACL value `posix_acl_from_xattr` or `posix_acl_valid` refuses
    /// (`EINVAL`).
    #[error("POSIX ACL value is malformed or its entries are invalid")]
    AclInvalid,
    /// A POSIX ACL value of a version other than `POSIX_ACL_XATTR_VERSION`
    /// (`EOPNOTSUPP`).
    #[error("POSIX ACL version {version} is not supported")]
    AclVersion { version: u32 },
    /// A POSIX ACL on a symlink, which has no ACL operations (`EOPNOTSUPP`).
    #[error("inode {inode} in tree {tree} is a symlink, which takes no POSIX ACL")]
    AclNotSupported { tree: u64, inode: u64 },
    /// A default ACL on a non-directory (`EACCES`, `set_posix_acl`).
    #[error("inode {inode} in tree {tree} is not a directory and takes no default ACL")]
    AclDefaultNotDirectory { tree: u64, inode: u64 },

    #[error("file offset or inode byte accounting overflows")]
    FileOverflow,
    #[error("file extent {key} is not a regular or preallocated mapping")]
    NotRegularExtent { key: Key },
    #[error("file range {start}..{end} must be nonempty and aligned to sector size {sector}")]
    FileRange { start: u64, end: u64, sector: u64 },
    #[error("writer must be reopened after a failed or interrupted commit")]
    WriterPoisoned,
    #[error("transaction superblock: {0}")]
    CommitSuperblock(#[from] crate::SuperblockDefect),
    #[error("filesystem changed after this snapshot was mounted")]
    StaleSnapshot,
    #[error("filesystem generation cannot be incremented")]
    GenerationOverflow,
    #[error("staging range at {logical} with {bytes} bytes is empty or not block-aligned")]
    StagingRange { logical: u64, bytes: usize },
    #[error(transparent)]
    Read(#[from] Error),
    #[error(transparent)]
    Space(#[from] crate::space::SpaceError),
    #[error(transparent)]
    Item(#[from] EditError),
    #[error("free-space item {key}: {defect}")]
    FreeSpace {
        key: Key,
        defect: crate::free_space::FreeSpaceDefect,
    },
    #[error("UUID item {key}: {defect}")]
    Uuid {
        key: Key,
        defect: crate::uuid_tree::UuidDefect,
    },
    #[error("allocation item {key}: {defect}")]
    Allocation {
        key: Key,
        defect: crate::allocation::AllocationDefect,
    },
    #[error("data allocation item {key}: {defect}")]
    Data {
        key: Key,
        defect: crate::allocation::data::DataDefect,
    },
    #[error("checksum item {key} has an invalid sector range or digest payload")]
    Checksum { key: Key },
    #[error("tree {tree} metadata usage {used} cannot add {added} and remove {removed} bytes")]
    RootUsage {
        tree: u64,
        used: u64,
        added: u64,
        removed: u64,
    },
    #[error("metadata extent for block {address} at level {level} is missing")]
    MissingExtent { address: u64, level: u8 },
    #[error("no metadata space remains for tree {tree}")]
    NoSpace { tree: u64 },
    #[error(transparent)]
    Chunk(#[from] crate::transaction::chunks::ChunkError),
    #[error("tree {tree} cannot grow beyond {MAX_LEVEL} levels")]
    Height { tree: u64 },
}

/// Allocation is transaction-owned: reservations, extent references and abort
/// cleanup all follow the same transaction lifetime, including failed edits.
pub(crate) trait MetadataAllocator {
    fn allocate(&mut self, tree: u64, level: u8, first: Key) -> Result<u64, WriteError>;

    /// Update the first key of a block that remains private to this transaction.
    fn revise(&mut self, address: u64, first: Key);

    /// Return an unpublished allocation after its overlay block is discarded.
    fn cancel(&mut self, address: u64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetiredBlock {
    pub(crate) tree: u64,
    pub(crate) address: u64,
    pub(crate) level: u8,
    pub(crate) generation: u64,
}

/// Retained overlay versions share immutable nodes and persistent collection
/// roots. Updating an address copies only its collection path; readers of the
/// prior version retain the prior block even when the address is reused.
#[derive(Clone)]
pub(crate) struct Trees {
    generation: u64,
    checksum: ChecksumType,
    blocks: RedBlackTreeMapSync<u64, Node>,
    retired: VectorSync<RetiredBlock>,
}

pub(crate) struct Overlay<'a, S> {
    volume: &'a Volume<S>,
    trees: &'a Trees,
}

impl<S: BlockSource> NodeSource for Overlay<'_, S> {
    async fn read_node(&self, logical: u64, expected: Expected) -> Result<Node, Error> {
        self.trees.read(self.volume, logical, expected).await
    }
}

impl Trees {
    pub(crate) fn reader<'a, S: BlockSource>(&'a self, volume: &'a Volume<S>) -> Overlay<'a, S> {
        Overlay {
            volume,
            trees: self,
        }
    }

    pub(crate) fn new(generation: u64, checksum: ChecksumType) -> Self {
        Self {
            generation,
            checksum,
            blocks: RedBlackTreeMapSync::new_sync(),
            retired: VectorSync::new_sync(),
        }
    }

    pub(crate) async fn read<S: BlockSource>(
        &self,
        volume: &Volume<S>,
        address: u64,
        expected: Expected,
    ) -> Result<Node, Error> {
        if let Some(node) = self.blocks.get(&address) {
            return Ok(node.clone());
        }
        Node::read(volume, address, expected).await
    }

    fn retire<A: MetadataAllocator>(
        &mut self,
        tree: u64,
        address: u64,
        node: &Node,
        allocator: &mut A,
    ) {
        if self.blocks.remove_mut(&address) {
            allocator.cancel(address);
        } else {
            let header: &RawHeader = view(&node.bytes).expect("validated retired node");
            self.retired.push_back_mut(RetiredBlock {
                tree,
                address,
                level: node.level,
                generation: header.generation.get(),
            });
        }
    }

    fn insert(&mut self, bytes: Arc<[u8]>) {
        let header: &RawHeader = view(&bytes).expect("encoded header");
        self.blocks.insert_mut(
            header.bytenr.get(),
            Node {
                level: header.level,
                nritems: header.nritems.get() as usize,
                bytes,
            },
        );
    }

    async fn path<S: BlockSource>(
        &self,
        volume: &Volume<S>,
        root: TreeRoot,
        key: Key,
    ) -> Result<ArrayVec<(u64, Node, usize), { MAX_LEVEL as usize }>, WriteError> {
        let mut path = ArrayVec::new();
        let mut address = root.bytenr;
        let mut expected = Expected {
            level: root.level,
            generation: root.generation,
            first_key: None,
        };
        loop {
            let node = self.read(volume, address, expected).await?;
            let slot = node.floor_slot(key);
            if node.level == 0 {
                path.push((address, node, slot));
                return Ok(path);
            }
            let (child, generation) = node.child(slot);
            expected = Expected {
                level: node.level - 1,
                generation,
                first_key: Some(node.key(slot)),
            };
            path.push((address, node, slot));
            address = child;
        }
    }

    /// Reads the transaction's current value, including unpublished edits.
    pub(crate) async fn get<S: BlockSource>(
        &self,
        volume: &Volume<S>,
        root: TreeRoot,
        key: Key,
    ) -> Result<Option<Vec<u8>>, WriteError> {
        let path = self.path(volume, root, key).await?;
        let (_, node, slot) = path.last().expect("path includes a leaf");
        Ok((node.nritems > 0 && node.key(*slot) == key).then(|| node.item(*slot).to_vec()))
    }

    pub(crate) async fn floor<S: BlockSource>(
        &self,
        volume: &Volume<S>,
        root: TreeRoot,
        key: Key,
    ) -> Result<Option<(Key, Vec<u8>)>, WriteError> {
        let path = self.path(volume, root, key).await?;
        let (_, leaf, slot) = path.last().expect("path includes leaf");
        Ok((leaf.nritems > 0 && leaf.key(*slot) <= key)
            .then(|| (leaf.key(*slot), leaf.item(*slot).to_vec())))
    }

    /// First item at or above a key in the current transaction. Ascending to
    /// the next sibling is necessary when the selected leaf ends below it.
    pub(crate) async fn ceiling<S: BlockSource>(
        &self,
        volume: &Volume<S>,
        root: TreeRoot,
        key: Key,
    ) -> Result<Option<(Key, Vec<u8>)>, WriteError> {
        let cursor = Cursor::ceiling(&self.reader(volume), root, key).await?;
        Ok(cursor
            .key()
            .zip(cursor.item())
            .map(|(key, bytes)| (key, bytes.to_vec())))
    }

    /// Applies one insertion, replacement, or deletion. On error the owner
    /// aborts the entire transaction, releasing all reservations; no partial
    /// root or block from this overlay has been published to readers.
    pub(crate) async fn apply<S: BlockSource, A: MetadataAllocator>(
        &mut self,
        volume: &Volume<S>,
        root: TreeRoot,
        key: Key,
        value: Option<&[u8]>,
        allocator: &mut A,
    ) -> Result<TreeRoot, WriteError> {
        let mut path = self.path(volume, root, key).await?;
        let original_root = path[0].1.clone();
        let (address, node, _) = path.pop().expect("path includes a leaf");
        let mut leaf = Leaf::from_node(&node);
        match value {
            Some(bytes) => leaf.set(key, bytes)?,
            None if !leaf.remove(key) => return Ok(root),
            None => {}
        }
        let mut children = Vec::new();
        let mut reusable = self.blocks.contains_key(&address).then_some(address);
        for range in leaf.partitions() {
            let Some(first) = leaf.first_key(&range) else {
                continue;
            };
            let fresh = destination(&mut reusable, allocator, root.tree, 0, first)?;
            self.insert(leaf.encode(range, fresh, self.generation, root.tree, self.checksum));
            children.push(Child {
                key: first,
                address: fresh,
                generation: self.generation,
            });
        }
        if children
            .first()
            .is_none_or(|child| child.address != address)
        {
            self.retire(root.tree, address, &node, allocator);
        }
        let mut level = 0;
        while let Some((address, node, slot)) = path.pop() {
            level = node.level;
            let mut branch = Branch::from_node(&node);
            branch.replace(slot, &children);
            children.clear();
            let mut reusable = self.blocks.contains_key(&address).then_some(address);
            for range in branch.partitions() {
                let Some(first) = branch.first_key(&range) else {
                    continue;
                };
                let fresh = destination(&mut reusable, allocator, root.tree, level, first)?;
                self.insert(branch.encode(range, fresh, self.generation, root.tree, self.checksum));
                children.push(Child {
                    key: first,
                    address: fresh,
                    generation: self.generation,
                });
            }
            if children
                .first()
                .is_none_or(|child| child.address != address)
            {
                self.retire(root.tree, address, &node, allocator);
            }
        }
        if children.is_empty() {
            let fresh = allocator.allocate(root.tree, 0, Key::new(0, 0, 0))?;
            let leaf = Leaf::empty(Header::for_level(&original_root, 0));
            self.insert(leaf.encode(0..0, fresh, self.generation, root.tree, self.checksum));
            return Ok(TreeRoot {
                tree: root.tree,
                bytenr: fresh,
                generation: self.generation,
                level: 0,
            });
        }
        while children.len() > 1 {
            level += 1;
            if level >= MAX_LEVEL {
                return Err(WriteError::Height { tree: root.tree });
            }
            let branch = Branch::new(
                Header::for_level(&original_root, level),
                core::mem::take(&mut children),
            );
            for range in branch.partitions() {
                let first = branch.first_key(&range).expect("nonempty new root");
                let fresh = allocator.allocate(root.tree, level, first)?;
                self.insert(branch.encode(range, fresh, self.generation, root.tree, self.checksum));
                children.push(Child {
                    key: first,
                    address: fresh,
                    generation: self.generation,
                });
            }
        }
        let mut child = children[0];
        // Remove a unary root after deletion; its only child already names a
        // complete subtree with the same key range and owner.
        while level > 0 {
            let node = self
                .read(
                    volume,
                    child.address,
                    Expected {
                        level,
                        generation: child.generation,
                        first_key: None,
                    },
                )
                .await?;
            if node.nritems != 1 {
                break;
            }
            let (address, generation) = node.child(0);
            self.retire(root.tree, child.address, &node, allocator);
            child = Child {
                key: node.key(0),
                address,
                generation,
            };
            level -= 1;
        }
        Ok(TreeRoot {
            tree: root.tree,
            bytenr: child.address,
            generation: child.generation,
            level,
        })
    }

    pub(crate) async fn create<S: BlockSource, A: MetadataAllocator>(
        &mut self,
        volume: &Volume<S>,
        template: TreeRoot,
        tree: u64,
        allocator: &mut A,
    ) -> Result<TreeRoot, WriteError> {
        let node = self
            .read(
                volume,
                template.bytenr,
                Expected {
                    level: template.level,
                    generation: template.generation,
                    first_key: None,
                },
            )
            .await?;
        let address = allocator.allocate(tree, 0, Key::new(0, 0, 0))?;
        let leaf = Leaf::empty(Header::new_tree(&node));
        self.insert(leaf.encode(0..0, address, self.generation, tree, self.checksum));
        Ok(TreeRoot {
            tree,
            bytenr: address,
            generation: self.generation,
            level: 0,
        })
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// How many private tree blocks the overlay holds: what its commit writes.
    pub(crate) fn block_count(&self) -> usize {
        self.blocks.size()
    }

    pub(crate) fn blocks(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.blocks
            .iter()
            .map(|(&address, node)| (address, &*node.bytes))
    }

    pub(crate) fn retired(&self) -> &VectorSync<RetiredBlock> {
        &self.retired
    }
}

/// An overlay block is unreachable from every published root. Retaining its
/// address lets allocation-tree accounting converge without changing the set of
/// reservations on each edit. Splits still reserve their additional blocks.
fn destination<A: MetadataAllocator>(
    reusable: &mut Option<u64>,
    allocator: &mut A,
    tree: u64,
    level: u8,
    first: Key,
) -> Result<u64, WriteError> {
    if let Some(address) = reusable.take() {
        allocator.revise(address, first);
        Ok(address)
    } else {
        allocator.allocate(tree, level, first)
    }
}
