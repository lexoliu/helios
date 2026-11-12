//! Memory tagging: an allocation tag stored with every granule of memory
//! reached through a tag-capable mapping, and a logical tag in a pointer's
//! top byte that the processor compares with it on access.
//!
//! # Concurrency contract
//!
//! Every operation is lock-free, allocates nothing and may run in fault or
//! interrupt context. It touches only the granules it is given and the
//! executing processor's own tag-generation registers, and the caller owns
//! those granules for the call. A tag-storing operation completes its stores
//! to the inner-shareable domain before it returns, so whatever release later
//! publishes the range to another processor publishes its tags with it.
//! Tag checking is uniform: a backend enables it on every processor before
//! [`Cpu::start_processor`](crate::cpu::Cpu::start_processor) returns for that
//! processor and refuses to start one that cannot check, so
//! [`AllocationTags::checking`] gives the same answer on every processor that
//! runs kernel code. [`TaggedView`] and [`MemoryTagging`] are immutable values
//! fixed on the bootstrap processor before the `Cpu` reaches the kernel.

use core::ptr::NonNull;

/// A four-bit tag: the allocation tag stored with a granule, or the logical
/// tag in a pointer's top byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag(u8);

impl Tag {
    /// The match-all tag of every pointer formed without a tagging call.
    pub const UNCHECKED: Self = Self(0xf);

    /// The tag of memory no live object owns, whether never written or
    /// released. A live object never carries it, so a pointer into memory
    /// that was never tagged or was released never matches one.
    pub const FREE: Self = Self(0);

    /// The tag whose value is `raw`, which is at most `0xf`.
    pub const fn new(raw: u8) -> Self {
        assert!(raw <= 0xf, "a tag is four bits");
        Self(raw)
    }

    /// The tag's value, at most `0xf`.
    pub const fn raw(self) -> u8 {
        self.0
    }
}

/// A set of tags, one bit per tag: the tags a choice must avoid. It is the
/// shape of the mask the hardware's tag generator takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagSet(u16);

impl TagSet {
    /// The set holding no tag.
    pub const EMPTY: Self = Self(0);

    /// This set with `tag` added.
    #[must_use]
    pub const fn with(self, tag: Tag) -> Self {
        Self(self.0 | 1 << tag.raw())
    }

    /// Whether `tag` is in the set.
    pub const fn contains(self, tag: Tag) -> bool {
        self.0 >> tag.raw() & 1 != 0
    }
}

/// The allocation-tag operations of the executing processor.
///
/// Associated functions, not methods: the consumers are reached from fault
/// context and `Drop` paths that hold no handle, the same reason the
/// processor index is a linkage contract.
pub trait AllocationTags: Send + Sync + 'static {
    /// Bytes one allocation tag covers; the ranges below are multiples of it.
    const GRANULE_BYTES: usize;

    /// Whether the executing processor checks tags.
    fn checking() -> bool;

    /// Chooses a tag outside [`Tag::UNCHECKED`], [`Tag::FREE`] and `avoid`,
    /// stores it into every granule of `range` and returns it. Returns
    /// [`Tag::UNCHECKED`] and stores nothing when [`Self::checking`] is false.
    ///
    /// # Safety
    ///
    /// `range` is granule-aligned, its addresses canonical, reached through a
    /// tag-capable mapping, and owned by the caller for the call.
    unsafe fn assign(range: NonNull<[u8]>, avoid: TagSet) -> Tag;

    /// Stores `tag` into every granule of `range`.
    ///
    /// # Safety
    ///
    /// As [`Self::assign`].
    unsafe fn store(range: NonNull<[u8]>, tag: Tag);

    /// `addr` carrying `tag`.
    fn apply(addr: usize, tag: Tag) -> usize;

    /// `addr` with any logical tag removed.
    fn strip(addr: usize) -> usize;
}

/// Hardware that stores no allocation tags: every operation is the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoAllocationTags;

impl AllocationTags for NoAllocationTags {
    const GRANULE_BYTES: usize = 1;

    fn checking() -> bool {
        false
    }

    unsafe fn assign(_range: NonNull<[u8]>, _avoid: TagSet) -> Tag {
        Tag::UNCHECKED
    }

    unsafe fn store(_range: NonNull<[u8]>, _tag: Tag) {}

    fn apply(addr: usize, _tag: Tag) -> usize {
        addr
    }

    fn strip(addr: usize) -> usize {
        addr
    }
}

/// A second linear view of the kernel's direct map, through tag-capable
/// mappings: `phys + tagged_offset` and `phys + direct_offset` name the same
/// byte for every mapped `phys < span`. [`TaggedView::direct_map`] is the view
/// a machine without tags uses: the direct map itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaggedView {
    direct_offset: usize,
    tagged_offset: usize,
    span: usize,
    line_bytes: usize,
}

impl TaggedView {
    /// The view of a machine without tags: the direct map at `direct_offset`
    /// is its own tagged view.
    pub const fn direct_map(direct_offset: usize) -> Self {
        Self {
            direct_offset,
            tagged_offset: direct_offset,
            span: usize::MAX,
            line_bytes: 1,
        }
    }

    /// The direct map at `direct_offset` and its tagged view at
    /// `tagged_offset`, each covering physical addresses below `span`, with
    /// seam objects padded to `line_bytes`, a power of two.
    pub const fn new(
        direct_offset: usize,
        tagged_offset: usize,
        span: usize,
        line_bytes: usize,
    ) -> Self {
        assert!(
            line_bytes.is_power_of_two(),
            "a line size is a power of two"
        );
        Self {
            direct_offset,
            tagged_offset,
            span,
            line_bytes,
        }
    }

    /// The tagged-view address of a direct-map address.
    pub fn tagged(&self, direct: usize) -> usize {
        direct - self.direct_offset + self.tagged_offset
    }

    /// The physical address of a canonical address in either view, or `None`
    /// when it lies in neither.
    pub fn physical(&self, canonical: usize) -> Option<u64> {
        [self.direct_offset, self.tagged_offset]
            .into_iter()
            .find_map(|offset| {
                canonical
                    .checked_sub(offset)
                    .filter(|physical| *physical < self.span)
            })
            .map(|physical| physical as u64)
    }

    /// Bytes a seam object reached through the view is aligned and padded to,
    /// so that it shares no cache-writeback granule with memory reached
    /// through the direct map; 1 for [`TaggedView::direct_map`].
    pub const fn line_bytes(&self) -> usize {
        self.line_bytes
    }
}

/// What a backend enabled, for the boot report. The value is fixed on the
/// bootstrap processor before the `Cpu` reaches the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryTagging {
    /// What every processor checks, or `None` when none does.
    pub checking: Option<TagChecking>,
}

impl MemoryTagging {
    /// A backend that enabled no memory tagging.
    pub const NONE: Self = Self { checking: None };
}

/// Synchronous checking at the kernel's privilege level: a mismatch raises an
/// exception at the faulting instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagChecking {
    /// Bytes one allocation tag covers.
    pub granule_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(addr: usize, len: usize) -> NonNull<[u8]> {
        NonNull::slice_from_raw_parts(NonNull::new(addr as *mut u8).unwrap(), len)
    }

    #[test]
    fn no_allocation_tags_is_the_identity() {
        let addr = 0xffff_8000_dead_b000;
        assert!(!NoAllocationTags::checking());
        assert_eq!(NoAllocationTags::apply(addr, Tag::new(0x5)), addr);
        assert_eq!(NoAllocationTags::strip(addr), addr);
        assert_eq!(
            NoAllocationTags::strip(NoAllocationTags::apply(addr, Tag::new(0x9))),
            addr
        );
        // SAFETY: the implementation never dereferences the range.
        let tag =
            unsafe { NoAllocationTags::assign(bytes(addr, 64), TagSet::EMPTY.with(Tag::new(0x3))) };
        assert_eq!(tag, Tag::UNCHECKED);
    }

    #[test]
    fn a_tag_set_holds_what_was_added() {
        let set = TagSet::EMPTY.with(Tag::new(0x3)).with(Tag::UNCHECKED);
        assert!(set.contains(Tag::new(0x3)));
        assert!(set.contains(Tag::UNCHECKED));
        assert!(!set.contains(Tag::FREE));
        assert!(!set.contains(Tag::new(0x4)));
        assert!((0..=0xf).all(|raw| !TagSet::EMPTY.contains(Tag::new(raw))));
    }

    #[test]
    fn tagged_view_translates_both_views() {
        const DIRECT: usize = 0xffff_8000_0000_0000;
        const TAGGED: usize = 0xffff_e000_0000_0000;
        const SPAN: usize = 0x8000_0000;
        let view = TaggedView::new(DIRECT, TAGGED, SPAN, 64);
        let phys = 0x1234_5000_usize;
        assert_eq!(view.physical(DIRECT + phys), Some(phys as u64));
        assert_eq!(view.physical(TAGGED + phys), Some(phys as u64));
        assert_eq!(view.tagged(DIRECT + phys), TAGGED + phys);
        assert_eq!(view.physical(view.tagged(DIRECT + phys)), Some(phys as u64));
        assert_eq!(view.physical(DIRECT + SPAN - 1), Some(SPAN as u64 - 1));
        assert_eq!(view.physical(DIRECT + SPAN), None);
        assert_eq!(view.physical(TAGGED + SPAN), None);
        assert_eq!(view.physical(DIRECT - 1), None);
        assert_eq!(view.line_bytes(), 64);
    }

    #[test]
    fn direct_map_view_is_the_direct_map() {
        const DIRECT: usize = 0xffff_8000_0000_0000;
        let view = TaggedView::direct_map(DIRECT);
        assert_eq!(view.tagged(DIRECT + 0x4000), DIRECT + 0x4000);
        assert_eq!(view.physical(DIRECT + 0x4000), Some(0x4000));
        assert_eq!(view.line_bytes(), 1);
    }

    #[test]
    fn memory_tagging_none_names_no_checking() {
        assert_eq!(MemoryTagging::NONE.checking, None);
        assert_ne!(
            MemoryTagging::NONE,
            MemoryTagging {
                checking: Some(TagChecking { granule_bytes: 16 }),
            }
        );
    }
}
