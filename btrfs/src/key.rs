//! Keys: the `(objectid, type, offset)` triple every tree is ordered by.

use core::fmt;

use crate::ondisk::RawKey;

/// A tree key. Keys compare by object id, then item type, then offset,
/// which is the order every btrfs tree keeps its items in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub objectid: u64,
    pub item_type: u8,
    pub offset: u64,
}

impl Key {
    pub const fn new(objectid: u64, item_type: u8, offset: u64) -> Self {
        Self {
            objectid,
            item_type,
            offset,
        }
    }

    pub(crate) fn from_raw(raw: &RawKey) -> Self {
        Self {
            objectid: raw.objectid.get(),
            item_type: raw.item_type,
            offset: raw.offset.get(),
        }
    }

    /// Whether the key belongs to object `objectid` and is of `item_type`.
    pub(crate) const fn is(&self, objectid: u64, item_type: u8) -> bool {
        self.objectid == objectid && self.item_type == item_type
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "({} {} {})",
            self.objectid, self.item_type, self.offset
        )
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}
