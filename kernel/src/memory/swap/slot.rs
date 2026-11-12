//! The swap slot table: one atomic word per swapped-out page.
//!
//! A page-table entry that swapped a page out carries one [`SwapToken`],
//! which is a [`SlotIndex`] into this table. The word at that index says
//! where the page's bytes are right now: nowhere (a zero page), in the
//! compressed store, parked in a whole store frame, in the swap backend,
//! or in somebody's hands.
//!
//! # Concurrency contract
//!
//! The table is shared: the swap owner task and every processor that
//! resolves a fault read and write it.
//!
//! - **Reading a word** ([`SlotTable::state`]) is one acquire load, on any
//!   processor.
//! - **A claim is an exclusive lease on a slot's storage.** [`SlotTable::claim`]
//!   compare-and-swaps a settled word to [`SlotState::Claimed`]; exactly one
//!   caller wins, and a loser learns the word it found. The holder reads or
//!   frees the storage with no other synchronisation, then either
//!   [`SlotTable::publish`]es a settled word (the storage moved, or the
//!   claim is being put back) or [`SlotTable::free`]s the slot. Holding a
//!   claim never blocks anyone: a processor that finds `Claimed` waits for
//!   progress, it does not spin on the word. A claim is held across no
//!   await on a fault path; the owner holds one across a backend transfer.
//! - **Ownership of the slot *index* moves with the page-table entry.** The
//!   entry is consumed exactly once, under the address space's reservation
//!   lock, by a successful reinstatement or by a discard. A claim is
//!   therefore never a claim on the index, only on the storage behind it,
//!   and whoever consumes the entry is the one that frees the index.
//! - **Reserving** ([`SlotAllocator::reserve`]) is the owner task's alone
//!   and gives it a [`Claim`] on a fresh or returned index, in the
//!   `Reserved` state. The allocator's own state is not shared.
//! - **Freeing** ([`SlotTable::free`]) runs on any processor: it pushes the
//!   index onto a lock-free return stack ([`super::stack`]) that only the
//!   owner takes from. A freed word is `Free { next }`, a link of that
//!   stack, so nothing is allocated to return a slot.
//! - **Leaves are allocated by the owner** when it first hands out an index
//!   in their range, published with release ordering, and never freed: the
//!   table does not shrink, and its size is bounded by the peak number of
//!   concurrently swapped pages at eight bytes each.
//!
//! The words are not cache-line padded: each has no designated writer,
//! whichever processor holds its claim writes it, and contention is per
//! page and brief. Padding would cost 64 to 128 bytes per swapped page,
//! more than the memory the tier exists to save.
//!
//! A word can carry the same bits for two pages in turn (a slot freed and
//! reused for another page that is stored identically). A claim made from a
//! stale read is caught by the token check the address space makes under
//! its lock, not by the word: see
//! [`helios_hal::vmm::AddressSpace::swap_in_page`].

extern crate alloc;

use alloc::boxed::Box;
use core::num::{NonZeroU16, NonZeroU32};
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::SwapToken;
use spin::Once;

use super::stack::{Chain, Links, ReturnStack};

/// Bytes of a store frame one bit of its occupancy bitmap stands for.
pub(super) const GRANULE_BYTES: usize = 64;
/// Granules in one store frame: one bit each in a 64-bit bitmap.
pub(super) const GRANULES_PER_FRAME: usize = PhysFrame::SIZE / GRANULE_BYTES;
/// Bytes ahead of an object's payload: its slot index, little endian.
pub(super) const OBJECT_HEADER_BYTES: usize = 4;

/// Slots in one leaf, and entries in one interior node.
const FANOUT: usize = 512;
const FANOUT_BITS: u32 = FANOUT.trailing_zeros();

/// The largest index the table holds: three levels of [`FANOUT`] entries,
/// less the unused index 0.
pub(super) const MAX_SLOT_INDEX: u32 = (1 << (3 * FANOUT_BITS)) - 1;

/// The value a page-table entry carries, as the kernel reads it:
/// `1..=`[`MAX_SLOT_INDEX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct SlotIndex(NonZeroU32);

impl SlotIndex {
    /// The index a token names, or `None` for a token past the table.
    pub(super) fn from_token(token: SwapToken) -> Option<Self> {
        Self::new(token.raw())
    }

    fn new(raw: u32) -> Option<Self> {
        if raw > MAX_SLOT_INDEX {
            return None;
        }
        NonZeroU32::new(raw).map(Self)
    }

    /// The token a page-table entry carries for this slot.
    pub(super) fn token(self) -> SwapToken {
        SwapToken::new(self.0.get()).expect("a slot index is never zero")
    }

    pub(super) fn raw(self) -> u32 {
        self.0.get()
    }
}

/// A store frame, by its index in the store's frame table. Zero is unused
/// so that a frame index can be a [`ReturnStack`] handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StoreFrameIndex(NonZeroU32);

/// Store frame indices fit the 28 bits a slot word has for them.
const FRAME_BITS: u32 = 28;

/// The first granule of an object in its frame, `0..`[`GRANULES_PER_FRAME`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Granule(u8);

/// Bytes of an LZ4 block that passed the compressibility gate.
///
/// There is no public constructor: [`Gate::accept`] is the only way to
/// make one outside this module's decoder, so a `Stored` slot can only
/// describe an object that passed the gate in force when it was stored.
/// "An incompressible page sitting in the store" has no representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CompressedLen(NonZeroU16);

/// Bits a slot word has for an object's length.
const LEN_BITS: u32 = 13;

/// Where a disk-resident page's backend token is kept: an index into the
/// owner's `DiskEntries`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DiskKey(u32);

impl DiskKey {
    pub(super) fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(super) fn raw(self) -> u32 {
        self.0
    }
}

/// Where an LZ4 object lives in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StoredObject {
    frame: StoreFrameIndex,
    first: Granule,
    len: CompressedLen,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the store that places objects is the next commit's"
    )
)]
impl StoreFrameIndex {
    /// The frame at `raw` in the store's frame table, or `None` for index
    /// 0 (unused) or one the slot word cannot carry.
    pub(super) fn new(raw: u32) -> Option<Self> {
        if u64::from(raw) >= 1 << FRAME_BITS {
            return None;
        }
        NonZeroU32::new(raw).map(Self)
    }

    pub(super) fn raw(self) -> u32 {
        self.0.get()
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the store that places objects is the next commit's"
    )
)]
impl Granule {
    /// The granule at `raw`, or `None` past the end of a frame.
    pub(super) fn new(raw: usize) -> Option<Self> {
        (raw < GRANULES_PER_FRAME).then_some(Self(raw as u8))
    }

    pub(super) fn raw(self) -> usize {
        usize::from(self.0)
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the store that places objects is the next commit's"
    )
)]
impl CompressedLen {
    /// Bytes of the compressed block, without its header.
    pub(super) fn get(self) -> usize {
        usize::from(self.0.get())
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the store that places objects is the next commit's"
    )
)]
impl StoredObject {
    pub(super) fn new(frame: StoreFrameIndex, first: Granule, len: CompressedLen) -> Self {
        Self { frame, first, len }
    }

    pub(super) fn frame(self) -> StoreFrameIndex {
        self.frame
    }

    pub(super) fn first(self) -> Granule {
        self.first
    }

    pub(super) fn len(self) -> CompressedLen {
        self.len
    }
}

/// The compressibility gate: what a page must compress to for the store to
/// take it.
///
/// A page is accepted when `ceil((header + n) / granule) <= accept_granules`
/// for `n` compressed bytes. The range of `accept_granules` is `1..=63`,
/// because a 64-granule object saves nothing; that bound comes from the
/// frame size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the eviction ladder that consults the gate is the next stage"
    )
)]
pub(super) struct Gate {
    accept_granules: u8,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the eviction ladder that consults the gate is the next stage"
    )
)]
impl Gate {
    /// A gate that accepts objects of up to `accept_granules` granules, or
    /// `None` outside `1..`[`GRANULES_PER_FRAME`].
    pub(super) fn new(accept_granules: u8) -> Option<Self> {
        (1..GRANULES_PER_FRAME as u8)
            .contains(&accept_granules)
            .then_some(Self { accept_granules })
    }

    /// The length of a compressed block, if it passes.
    pub(super) fn accept(&self, compressed_len: usize) -> Option<CompressedLen> {
        let granules = (OBJECT_HEADER_BYTES + compressed_len).div_ceil(GRANULE_BYTES);
        if granules > usize::from(self.accept_granules) {
            return None;
        }
        // At most 63 granules less the header: 4028 bytes, 12 bits.
        NonZeroU16::new(compressed_len as u16).map(CompressedLen)
    }
}

/// What a slot's word says, decoded.
///
/// This is an enum because it is a datum read out of a word that hardware
/// atomics serialise, not variation in behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotState {
    /// On the owner's chain or a return stack. `next` is the link of that
    /// stack. A slot that was never handed out reads as `Free` too.
    Free { next: Option<NonZeroU32> },
    /// The owner has the index and is detaching or placing the page.
    Reserved,
    /// The page was all zero: no storage.
    Zero,
    /// An LZ4 object in the compressed store.
    Stored(StoredObject),
    /// A raw page parked in a whole store frame.
    Parked(StoreFrameIndex),
    /// The page is in the swap backend, under the token at this key in the
    /// owner's `DiskEntries`.
    Disk(DiskKey),
    /// An exclusive lease on the storage is held by a fault or the owner.
    Claimed,
    /// A page-table reference is outstanding but there is no storage: its
    /// extent was consumed by a read whose page then went away. The orphan
    /// drain frees the index.
    Vacant,
    /// The page's bytes are gone: its address space refused the
    /// reinstatement and the backend refused the page back. The index
    /// stays reserved, because the entry names it, until the address space
    /// is torn down.
    Lost,
}

const TAG_MASK: u64 = 0xf;
const TAG_FREE: u64 = 0;
const TAG_RESERVED: u64 = 1;
const TAG_ZERO: u64 = 2;
const TAG_STORED: u64 = 3;
const TAG_PARKED: u64 = 4;
const TAG_DISK: u64 = 5;
const TAG_CLAIMED: u64 = 6;
const TAG_VACANT: u64 = 7;
const TAG_LOST: u64 = 8;

// The word, low to high: a 4-bit tag; for `Stored` and `Parked` the frame
// in bits 4..32; for `Stored` the first granule in bits 32..38 and the
// length in bits 38..51; for `Free` and `Disk` a 32-bit link or key in bits
// 32..64.
const FRAME_SHIFT: u32 = 4;
const GRANULE_SHIFT: u32 = 32;
const GRANULE_BITS: u32 = 6;
const LEN_SHIFT: u32 = 38;
const WIDE_SHIFT: u32 = 32;

const fn mask(bits: u32) -> u64 {
    (1 << bits) - 1
}

impl SlotState {
    fn encode(self) -> u64 {
        match self {
            Self::Free { next } => {
                TAG_FREE | (u64::from(next.map_or(0, NonZeroU32::get)) << WIDE_SHIFT)
            }
            Self::Reserved => TAG_RESERVED,
            Self::Zero => TAG_ZERO,
            Self::Stored(object) => {
                TAG_STORED
                    | (u64::from(object.frame.0.get()) << FRAME_SHIFT)
                    | (u64::from(object.first.0) << GRANULE_SHIFT)
                    | (u64::from(object.len.0.get()) << LEN_SHIFT)
            }
            Self::Parked(frame) => TAG_PARKED | (u64::from(frame.0.get()) << FRAME_SHIFT),
            Self::Disk(key) => TAG_DISK | (u64::from(key.0) << WIDE_SHIFT),
            Self::Claimed => TAG_CLAIMED,
            Self::Vacant => TAG_VACANT,
            Self::Lost => TAG_LOST,
        }
    }

    fn decode(word: u64) -> Self {
        let frame = || {
            let raw = ((word >> FRAME_SHIFT) & mask(FRAME_BITS)) as u32;
            StoreFrameIndex(
                NonZeroU32::new(raw)
                    .unwrap_or_else(|| panic!("slot word {word:#x} names store frame 0")),
            )
        };
        match word & TAG_MASK {
            TAG_FREE => Self::Free {
                next: NonZeroU32::new((word >> WIDE_SHIFT) as u32),
            },
            TAG_RESERVED => Self::Reserved,
            TAG_ZERO => Self::Zero,
            TAG_STORED => Self::Stored(StoredObject {
                frame: frame(),
                first: Granule(((word >> GRANULE_SHIFT) & mask(GRANULE_BITS)) as u8),
                len: CompressedLen(
                    NonZeroU16::new(((word >> LEN_SHIFT) & mask(LEN_BITS)) as u16)
                        .unwrap_or_else(|| panic!("slot word {word:#x} stores an empty object")),
                ),
            }),
            TAG_PARKED => Self::Parked(frame()),
            TAG_DISK => Self::Disk(DiskKey((word >> WIDE_SHIFT) as u32)),
            TAG_CLAIMED => Self::Claimed,
            TAG_VACANT => Self::Vacant,
            TAG_LOST => Self::Lost,
            tag => panic!("slot word {word:#x} carries the unknown tag {tag}"),
        }
    }

    /// Whether a claim may be taken on this state: it is settled, so some
    /// path owns the storage behind it by the protocol.
    fn is_settled(self) -> bool {
        !matches!(self, Self::Free { .. } | Self::Reserved | Self::Claimed)
    }
}

/// One slot's atomic word. Encoded and decoded only through
/// [`SlotState`].
#[repr(transparent)]
struct SlotWord(AtomicU64);

impl SlotWord {
    /// A word that reads `Free { next: None }`.
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn load(&self) -> SlotState {
        SlotState::decode(self.0.load(Ordering::Acquire))
    }

    fn store(&self, state: SlotState) {
        self.0.store(state.encode(), Ordering::Release);
    }

    fn compare_exchange(&self, current: SlotState, new: SlotState) -> Result<(), SlotState> {
        self.0
            .compare_exchange(
                current.encode(),
                new.encode(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(SlotState::decode)
    }
}

type Leaf = [SlotWord; FANOUT];

/// An interior node of the radix: [`FANOUT`] leaves, each allocated when
/// the owner first hands out an index in its range.
struct Mid(Box<[Once<Box<Leaf>>]>);

impl Mid {
    fn new() -> Self {
        Self((0..FANOUT).map(|_| Once::new()).collect())
    }
}

/// The exclusive lease on one slot: the owner's, from
/// [`SlotAllocator::reserve`], or a claimant's, from [`SlotTable::claim`].
///
/// A claim is resolved by [`SlotTable::publish`] or [`SlotTable::free`].
/// It is deliberately not `Clone` or `Copy`, and carries no borrow, so the
/// owner can hold one across a backend transfer while it mutates its own
/// state.
#[must_use = "a claim holds its slot until it is published or freed"]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Claim(SlotIndex);

impl Claim {
    pub(super) fn index(&self) -> SlotIndex {
        self.0
    }
}

/// The shared half of the slot table: the words, and the stack slots are
/// returned on.
pub(super) struct SlotTable {
    top: Box<[Once<Mid>]>,
    returned: ReturnStack,
}

impl SlotTable {
    pub(super) fn new() -> Self {
        Self {
            top: (0..FANOUT).map(|_| Once::new()).collect(),
            returned: ReturnStack::new(),
        }
    }

    /// The word at `index`, if its leaf exists.
    fn word(&self, index: SlotIndex) -> Option<&SlotWord> {
        let raw = index.raw() as usize;
        let mid = self.top[raw >> (2 * FANOUT_BITS)].get()?;
        let leaf = mid.0[(raw >> FANOUT_BITS) & (FANOUT - 1)].get()?;
        Some(&leaf[raw & (FANOUT - 1)])
    }

    /// The word at `index`, creating its leaf. Called by the owner when it
    /// first hands out an index in the leaf's range.
    fn word_or_create(&self, index: SlotIndex) -> &SlotWord {
        let raw = index.raw() as usize;
        let mid = self.top[raw >> (2 * FANOUT_BITS)].call_once(Mid::new);
        let leaf = mid.0[(raw >> FANOUT_BITS) & (FANOUT - 1)]
            .call_once(|| Box::new(core::array::from_fn(|_| SlotWord::new())));
        &leaf[raw & (FANOUT - 1)]
    }

    fn expect_word(&self, index: SlotIndex) -> &SlotWord {
        self.word(index)
            .unwrap_or_else(|| panic!("slot {} has no leaf: it was never handed out", index.raw()))
    }

    /// What the word at `index` says. An index never handed out reads as
    /// free.
    pub(super) fn state(&self, index: SlotIndex) -> SlotState {
        self.word(index)
            .map_or(SlotState::Free { next: None }, SlotWord::load)
    }

    /// Takes the exclusive lease on a settled slot whose word reads
    /// `expected`, or reports the word it found instead.
    ///
    /// # Panics
    ///
    /// In a debug build, if `expected` is `Free`, `Reserved` or `Claimed`:
    /// no path owns the storage behind those.
    pub(super) fn claim(&self, index: SlotIndex, expected: SlotState) -> Result<Claim, SlotState> {
        debug_assert!(
            expected.is_settled(),
            "slot {} claimed from {expected:?}, which is not a settled state",
            index.raw()
        );
        let Some(word) = self.word(index) else {
            return Err(SlotState::Free { next: None });
        };
        word.compare_exchange(expected, SlotState::Claimed)
            .map(|()| Claim(index))
    }

    /// Ends a claim by writing the settled state the slot is now in: the
    /// storage moved, or the slot is being put back as it was.
    ///
    /// # Panics
    ///
    /// In a debug build, if `state` is `Free`, `Reserved` or `Claimed`:
    /// freeing goes through [`Self::free`].
    pub(super) fn publish(&self, claim: Claim, state: SlotState) {
        debug_assert!(
            state.is_settled(),
            "slot {} published as {state:?}, which is not a settled state",
            claim.0.raw()
        );
        self.expect_word(claim.0).store(state);
    }

    /// Ends a claim by giving the index back: the page is resident again,
    /// or its storage and its page-table reference are both gone.
    pub(super) fn free(&self, claim: Claim) {
        self.returned.push(self, claim.0.0);
    }
}

impl Links for SlotTable {
    fn next_of(&self, handle: NonZeroU32) -> Option<NonZeroU32> {
        let index = SlotIndex(handle);
        match self.state(index) {
            SlotState::Free { next } => next,
            other => panic!(
                "slot {} is linked on a return stack but reads {other:?}",
                index.raw()
            ),
        }
    }

    fn set_next(&self, handle: NonZeroU32, next: Option<NonZeroU32>) {
        self.expect_word(SlotIndex(handle))
            .store(SlotState::Free { next });
    }
}

/// The owner task's private half of slot allocation: the chain of indices
/// it has taken back and the next index never handed out.
pub(super) struct SlotAllocator {
    local: Chain,
    next_fresh: u32,
}

impl SlotAllocator {
    pub(super) const fn new() -> Self {
        Self {
            local: Chain::empty(),
            // Index 0 is never handed out: `SwapToken` is non-zero so an
            // all-zero page-table entry keeps meaning "nothing was ever
            // mapped here".
            next_fresh: 1,
        }
    }

    /// Takes an index for a page about to be detached, in the `Reserved`
    /// state, or `None` when the table is full.
    ///
    /// An index freed since is reused before the table grows. The index
    /// has to exist before the page can be detached: the page-table entry
    /// carries it.
    pub(super) fn reserve(&mut self, table: &SlotTable) -> Option<Claim> {
        if self.local.is_empty() {
            self.local = table.returned.take_all();
        }
        let index = match self.local.pop(table) {
            Some(handle) => SlotIndex(handle),
            None => {
                let index = SlotIndex::new(self.next_fresh)?;
                self.next_fresh += 1;
                index
            }
        };
        table.word_or_create(index).store(SlotState::Reserved);
        Some(Claim(index))
    }
}
