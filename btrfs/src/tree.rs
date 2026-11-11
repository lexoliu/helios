//! Tree blocks and the walks over them.
//!
//! A [`Node`] exists only once its block has passed every check: the
//! checksum, the address and filesystem it records, its level and
//! generation against what its parent promised, the item count against the
//! block size, ascending keys, and leaf item data that lies inside the
//! block in the order the format lays it out. Its accessors index without
//! further checks because construction proved every index in range.
//!
//! A [`Cursor`] walks one tree in key order. Its depth is bounded by the
//! format's eight levels because every child must sit exactly one level
//! below its parent, which also makes a descent that loops impossible. A
//! walk across leaves checks that every key it yields is above the one
//! before; a block reachable twice, or a subtree whose keys escape the
//! range its parent gives it, breaks that order and ends the walk with
//! [`Error::TreeOrder`] instead of revisiting blocks.

use alloc::boxed::Box;
use alloc::vec;

use arrayvec::ArrayVec;

use crate::error::{Error, NodeDefect};
use crate::key::Key;
use crate::ondisk::{CSUM_FIELD_BYTES, MAX_LEVEL, RawHeader, RawItem, RawKey, RawKeyPtr, view};
use crate::source::BlockSource;
use crate::superblock::Superblock;
use crate::volume::Volume;

const HEADER_BYTES: usize = size_of::<RawHeader>();
const ITEM_BYTES: usize = size_of::<RawItem>();
const KEY_PTR_BYTES: usize = size_of::<RawKeyPtr>();

/// Where a tree starts, as the superblock or a root item records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRoot {
    /// The tree's object id: 1 for the root tree, 5 for the top-level
    /// subvolume, 256 and up for other subvolumes.
    pub tree: u64,
    pub(crate) bytenr: u64,
    pub(crate) level: u8,
    pub(crate) generation: u64,
}

/// What a parent promises about a child block before it is read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Expected {
    pub(crate) level: u8,
    pub(crate) generation: u64,
    /// The key the parent indexes the child under, which must be the
    /// child's first key. `None` for a tree's root.
    pub(crate) first_key: Option<Key>,
}

/// A validated tree block.
pub(crate) struct Node {
    bytes: Box<[u8]>,
    level: u8,
    nritems: usize,
}

impl Node {
    /// Reads the block at `logical` and validates it, trying each copy of
    /// a `dup` chunk until one passes.
    pub(crate) async fn read<S: BlockSource>(
        volume: &Volume<S>,
        logical: u64,
        expected: Expected,
    ) -> Result<Self, Error> {
        let superblock = volume.superblock();
        let nodesize = superblock.nodesize() as usize;
        let mut bytes = vec![0_u8; nodesize].into_boxed_slice();
        let nritems = volume
            .read_verified(logical, &mut bytes, |block| {
                validate(block, logical, expected, superblock)
                    .map_err(|defect| Error::TreeBlock { logical, defect })
            })
            .await?;
        Ok(Self {
            bytes,
            level: expected.level,
            nritems,
        })
    }

    pub(crate) fn level(&self) -> u8 {
        self.level
    }

    pub(crate) fn nritems(&self) -> usize {
        self.nritems
    }

    fn raw_key(&self, slot: usize) -> &RawKey {
        let stride = if self.level == 0 {
            ITEM_BYTES
        } else {
            KEY_PTR_BYTES
        };
        // In bounds: `validate` checked the item array fits the block.
        view(&self.bytes[HEADER_BYTES + slot * stride..]).expect("validated item array")
    }

    pub(crate) fn key(&self, slot: usize) -> Key {
        Key::from_raw(self.raw_key(slot))
    }

    /// The data of leaf item `slot`.
    pub(crate) fn item(&self, slot: usize) -> &[u8] {
        let item: &RawItem =
            view(&self.bytes[HEADER_BYTES + slot * ITEM_BYTES..]).expect("validated item array");
        let start = HEADER_BYTES + item.offset.get() as usize;
        &self.bytes[start..start + item.size.get() as usize]
    }

    /// The block and generation internal slot `slot` points at.
    fn child(&self, slot: usize) -> (u64, u64) {
        let ptr: &RawKeyPtr = view(&self.bytes[HEADER_BYTES + slot * KEY_PTR_BYTES..])
            .expect("validated key pointer array");
        (ptr.blockptr.get(), ptr.generation.get())
    }

    /// The last slot whose key is at most `target`, or slot 0 when every
    /// key is above it.
    fn floor_slot(&self, target: Key) -> usize {
        let (mut low, mut high) = (0, self.nritems);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.key(middle) <= target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.saturating_sub(1)
    }
}

/// Every check a tree block must pass. Returns the item count.
fn validate(
    bytes: &[u8],
    logical: u64,
    expected: Expected,
    superblock: &Superblock,
) -> Result<usize, NodeDefect> {
    let header: &RawHeader = view(bytes).ok_or(NodeDefect::ItemCount { nritems: 0 })?;
    if !superblock
        .csum_type()
        .verifies(&bytes[CSUM_FIELD_BYTES..], &header.csum)
    {
        return Err(NodeDefect::Checksum);
    }
    if header.bytenr.get() != logical {
        return Err(NodeDefect::Bytenr {
            recorded: header.bytenr.get(),
        });
    }
    if header.fsid != superblock.metadata_fsid() {
        return Err(NodeDefect::Fsid);
    }
    if header.level >= MAX_LEVEL || header.level != expected.level {
        return Err(NodeDefect::Level {
            expected: expected.level,
            found: header.level,
        });
    }
    let generation = header.generation.get();
    if generation > superblock.generation() {
        return Err(NodeDefect::FutureGeneration {
            found: generation,
            superblock: superblock.generation(),
        });
    }
    if generation != expected.generation {
        return Err(NodeDefect::Generation {
            expected: expected.generation,
            found: generation,
        });
    }

    let nritems_raw = header.nritems.get();
    let nritems = nritems_raw as usize;
    let stride = if header.level == 0 {
        ITEM_BYTES
    } else {
        KEY_PTR_BYTES
    };
    let capacity = (bytes.len() - HEADER_BYTES) / stride;
    if nritems > capacity {
        return Err(NodeDefect::ItemCount {
            nritems: nritems_raw,
        });
    }
    let is_root = expected.first_key.is_none();
    if nritems == 0 && (header.level > 0 || !is_root) {
        return Err(NodeDefect::Empty);
    }

    let key_at = |slot: usize| {
        let raw: &RawKey = view(&bytes[HEADER_BYTES + slot * stride..]).expect("checked capacity");
        Key::from_raw(raw)
    };
    for slot in 1..nritems {
        if key_at(slot - 1) >= key_at(slot) {
            return Err(NodeDefect::KeyOrder { slot: slot as u32 });
        }
    }
    if let Some(first) = expected.first_key
        && key_at(0) != first
    {
        return Err(NodeDefect::FirstKey {
            expected: first,
            found: key_at(0),
        });
    }

    if header.level == 0 {
        // Item data is packed from the end of the block toward the item
        // array, item 0 last: each item ends where the one before begins.
        let data_area = bytes.len() - HEADER_BYTES;
        let array_end = nritems * ITEM_BYTES;
        let mut end = data_area;
        for slot in 0..nritems {
            let item: &RawItem =
                view(&bytes[HEADER_BYTES + slot * ITEM_BYTES..]).expect("checked capacity");
            let offset = item.offset.get() as usize;
            let size = item.size.get() as usize;
            let fits = offset.checked_add(size) == Some(end) && offset >= array_end;
            if !fits {
                return Err(NodeDefect::ItemData { slot: slot as u32 });
            }
            end = offset;
        }
    }
    Ok(nritems)
}

struct Level {
    node: Node,
    slot: usize,
}

/// A position in one tree, walked forward in key order.
pub(crate) struct Cursor {
    tree: u64,
    /// The root first, the leaf last.
    path: ArrayVec<Level, { MAX_LEVEL as usize }>,
    /// Past the last item of the tree.
    exhausted: bool,
}

impl Cursor {
    /// A cursor on the last item whose key is at most `target`, or on the
    /// tree's first item when every key is above `target`.
    pub(crate) async fn floor<S: BlockSource>(
        volume: &Volume<S>,
        root: TreeRoot,
        target: Key,
    ) -> Result<Self, Error> {
        let mut path = ArrayVec::new();
        let mut node = Node::read(
            volume,
            root.bytenr,
            Expected {
                level: root.level,
                generation: root.generation,
                first_key: None,
            },
        )
        .await?;
        loop {
            let slot = node.floor_slot(target);
            if node.level() == 0 {
                let exhausted = node.nritems() == 0;
                path.push(Level { node, slot });
                return Ok(Self {
                    tree: root.tree,
                    path,
                    exhausted,
                });
            }
            let child = read_child(volume, &node, slot).await?;
            path.push(Level { node, slot });
            node = child;
        }
    }

    /// A cursor on the first item whose key is at least `target`.
    pub(crate) async fn ceiling<S: BlockSource>(
        volume: &Volume<S>,
        root: TreeRoot,
        target: Key,
    ) -> Result<Self, Error> {
        let mut cursor = Self::floor(volume, root, target).await?;
        if cursor.key().is_some_and(|key| key < target) {
            cursor.advance(volume).await?;
        }
        Ok(cursor)
    }

    /// The key under the cursor, or `None` past the end of the tree.
    pub(crate) fn key(&self) -> Option<Key> {
        let leaf = self.leaf()?;
        Some(leaf.node.key(leaf.slot))
    }

    /// The data of the item under the cursor.
    pub(crate) fn item(&self) -> Option<&[u8]> {
        let leaf = self.leaf()?;
        Some(leaf.node.item(leaf.slot))
    }

    fn leaf(&self) -> Option<&Level> {
        if self.exhausted {
            return None;
        }
        self.path.last()
    }

    pub(crate) fn tree(&self) -> u64 {
        self.tree
    }

    /// Moves to the next item in key order.
    pub(crate) async fn advance<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
    ) -> Result<(), Error> {
        let Some(previous) = self.key() else {
            return Ok(());
        };
        let leaf = self.path.last_mut().expect("a cursor always holds a leaf");
        if leaf.slot + 1 < leaf.node.nritems() {
            leaf.slot += 1;
            return Ok(());
        }

        // Climb to the lowest ancestor with a slot to the right.
        let Some(depth) = self
            .path
            .iter()
            .rposition(|level| level.node.level() > 0 && level.slot + 1 < level.node.nritems())
        else {
            self.exhausted = true;
            return Ok(());
        };
        self.path.truncate(depth + 1);
        let ancestor = self.path.last_mut().expect("depth is in the path");
        ancestor.slot += 1;

        // Descend along the leftmost edge.
        while let Some(parent) = self.path.last()
            && parent.node.level() > 0
        {
            let child = read_child(volume, &parent.node, parent.slot).await?;
            self.path.push(Level {
                node: child,
                slot: 0,
            });
        }
        let next = self.key().expect("a non-root block holds items");
        if next <= previous {
            return Err(Error::TreeOrder {
                tree: self.tree,
                previous,
                next,
            });
        }
        Ok(())
    }
}

async fn read_child<S: BlockSource>(
    volume: &Volume<S>,
    parent: &Node,
    slot: usize,
) -> Result<Node, Error> {
    let (bytenr, generation) = parent.child(slot);
    Node::read(
        volume,
        bytenr,
        Expected {
            level: parent.level() - 1,
            generation,
            first_key: Some(parent.key(slot)),
        },
    )
    .await
}
