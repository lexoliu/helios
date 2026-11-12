//! The slot table against its contract: the word round-trips every state,
//! indices are handed out and reused as the owner expects, and a claim is
//! exclusive between processors.

use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use helios_hal::vmm::SwapToken;

use super::slot::{
    CompressedLen, DiskKey, Gate, Granule, MAX_SLOT_INDEX, SlotAllocator, SlotIndex, SlotState,
    SlotTable, StoreFrameIndex, StoredObject,
};
use super::stack::Links;

fn gate(accept_granules: u8) -> Gate {
    Gate::new(accept_granules).expect("a granule count within a frame")
}

fn len(compressed_len: usize) -> CompressedLen {
    gate(63)
        .accept(compressed_len)
        .expect("a length that fits a frame")
}

fn nonzero(raw: u32) -> NonZeroU32 {
    NonZeroU32::new(raw).expect("non-zero")
}

fn object(frame: u32, first: usize, compressed_len: usize) -> StoredObject {
    StoredObject::new(
        StoreFrameIndex::new(frame).expect("a store frame"),
        Granule::new(first).expect("a granule in the frame"),
        len(compressed_len),
    )
}

#[test]
fn every_state_round_trips_through_its_word() {
    let table = SlotTable::new();
    let mut allocator = SlotAllocator::new();

    let settled = [
        SlotState::Zero,
        SlotState::Vacant,
        SlotState::Lost,
        SlotState::Disk(DiskKey::new(0)),
        SlotState::Disk(DiskKey::new(u32::MAX)),
        SlotState::Parked(StoreFrameIndex::new(1).unwrap()),
        SlotState::Parked(StoreFrameIndex::new((1 << 28) - 1).unwrap()),
        SlotState::Stored(object(1, 0, 1)),
        SlotState::Stored(object((1 << 28) - 1, 63, 4028)),
        SlotState::Stored(object(77, 31, 2876)),
    ];
    for state in settled {
        let claim = allocator.reserve(&table).expect("a slot");
        let index = claim.index();
        assert_eq!(table.state(index), SlotState::Reserved);
        table.publish(claim, state);
        assert_eq!(table.state(index), state, "{state:?} survives its word");

        let claim = table.claim(index, state).expect("the claim is free");
        assert_eq!(table.state(index), SlotState::Claimed);
        table.publish(claim, state);
        assert_eq!(table.state(index), state, "{state:?} survives a claim");
    }

    // A free word carries the next link of the stack it is on.
    let claim = allocator.reserve(&table).expect("a slot");
    let index = claim.index();
    table.publish(claim, SlotState::Zero);
    for next in [None, Some(nonzero(1)), Some(nonzero(u32::MAX))] {
        table.set_next(index_handle(index), next);
        assert_eq!(table.state(index), SlotState::Free { next });
    }
}

fn index_handle(index: SlotIndex) -> NonZeroU32 {
    nonzero(index.raw())
}

#[test]
fn a_stored_object_reads_back_what_was_stored() {
    let object = object(0x0abc_def0 & ((1 << 28) - 1), 17, 1234);
    assert_eq!(object.frame().raw(), 0x0abc_def0 & ((1 << 28) - 1));
    assert_eq!(object.first().raw(), 17);
    assert_eq!(object.len().get(), 1234);
}

#[test]
fn the_gate_is_the_only_way_to_a_length_and_never_exceeds_its_granules() {
    assert!(Gate::new(0).is_none(), "a zero-granule gate takes nothing");
    assert!(
        Gate::new(64).is_none(),
        "a 64-granule object saves nothing, so the range stops at 63"
    );

    let gate = gate(45);
    // 45 granules of 64 B, less the 4-byte header.
    assert!(gate.accept(2876).is_some());
    assert!(gate.accept(2877).is_none());
    assert!(gate.accept(4526).is_none(), "an incompressible page");
    assert!(gate.accept(0).is_none(), "an empty block is not an object");

    for accept_granules in 1..=63_u8 {
        let gate = super::slot::Gate::new(accept_granules).unwrap();
        for compressed_len in 0..=4526 {
            if let Some(accepted) = gate.accept(compressed_len) {
                assert_eq!(accepted.get(), compressed_len);
                assert!(
                    (4 + compressed_len).div_ceil(64) <= usize::from(accept_granules),
                    "accepted {compressed_len} B past {accept_granules} granules"
                );
                assert!(accepted.get() < 1 << 13, "the word has 13 bits for it");
            }
        }
    }
}

#[test]
fn a_store_frame_or_granule_the_word_cannot_carry_is_refused() {
    assert!(StoreFrameIndex::new(0).is_none(), "frame 0 is unused");
    assert!(StoreFrameIndex::new(1 << 28).is_none());
    assert!(Granule::new(64).is_none());
    assert!(Granule::new(63).is_some());
}

#[test]
fn index_zero_is_never_handed_out_and_the_table_bounds_what_a_token_may_name() {
    let table = SlotTable::new();
    let mut allocator = SlotAllocator::new();
    let first = allocator.reserve(&table).expect("a slot");
    assert_eq!(first.index().raw(), 1, "the first index is 1, never 0");

    let token = |raw| SwapToken::new(raw).expect("non-zero");
    assert!(SlotIndex::from_token(token(MAX_SLOT_INDEX)).is_some());
    assert!(SlotIndex::from_token(token(MAX_SLOT_INDEX + 1)).is_none());
    assert!(SlotIndex::from_token(token(u32::MAX)).is_none());
}

#[test]
fn a_freed_index_is_reused_before_the_table_grows() {
    let table = SlotTable::new();
    let mut allocator = SlotAllocator::new();
    let first = allocator.reserve(&table).expect("a slot");
    let second = allocator.reserve(&table).expect("a slot");
    assert_ne!(first.index(), second.index());

    let freed = first.index();
    table.free(first);
    assert_eq!(table.state(freed), SlotState::Free { next: None });
    let third = allocator.reserve(&table).expect("a slot");
    assert_eq!(third.index(), freed, "a freed index comes back first");
    assert_eq!(table.state(freed), SlotState::Reserved);
    table.free(third);
    table.free(second);
}

#[test]
fn indices_past_a_leaf_and_an_interior_node_are_handed_out_and_found() {
    let table = SlotTable::new();
    let mut allocator = SlotAllocator::new();
    // Past 512 (one leaf) and past 512 * 512 (one interior node).
    let count = 512 * 512 + 600;
    let mut claims = Vec::with_capacity(count);
    for _ in 0..count {
        claims.push(allocator.reserve(&table).expect("a slot"));
    }
    let last = claims.last().expect("claims").index();
    assert_eq!(last.raw() as usize, count);
    assert_eq!(table.state(last), SlotState::Reserved);
    for claim in claims {
        table.publish(claim, SlotState::Zero);
    }
    assert_eq!(table.state(last), SlotState::Zero);
    assert_eq!(
        table.state(SlotIndex::from_token(SwapToken::new(MAX_SLOT_INDEX).unwrap()).unwrap()),
        SlotState::Free { next: None },
        "an index in a leaf nobody allocated reads as free"
    );
}

#[test]
fn a_claim_is_exclusive_until_it_is_published() {
    let table = SlotTable::new();
    let mut allocator = SlotAllocator::new();
    let reserved = allocator.reserve(&table).expect("a slot");
    let index = reserved.index();
    table.publish(reserved, SlotState::Zero);

    let held = table
        .claim(index, SlotState::Zero)
        .expect("the first claim");
    assert_eq!(
        table.claim(index, SlotState::Zero).unwrap_err(),
        SlotState::Claimed,
        "a second claim learns the slot is held"
    );
    table.publish(held, SlotState::Zero);
    let again = table.claim(index, SlotState::Zero).expect("free again");
    assert_eq!(
        table.claim(index, SlotState::Vacant).unwrap_err(),
        SlotState::Claimed
    );
    table.publish(again, SlotState::Lost);
    assert_eq!(
        table.claim(index, SlotState::Zero).unwrap_err(),
        SlotState::Lost,
        "a claim from a word that has moved on reports the word it found"
    );
}

/// A fault and a discard race for one page: the fault claims the slot and
/// reinstates, the discard consumes the page-table entry and frees the
/// slot. Exactly one of them frees the storage and the index, whichever
/// way the interleaving falls.
///
/// The model: `entry` is the page-table entry's swap reference, consumed
/// exactly once (under the address space's lock, in the real thing); the
/// fault's reinstatement succeeds only if it is the one to consume it.
#[test]
fn a_fault_and_an_orphan_release_free_one_slot_exactly_once() {
    for _ in 0..10_000 {
        let table = SlotTable::new();
        let mut allocator = SlotAllocator::new();
        let reserved = allocator.reserve(&table).expect("a slot");
        let index = reserved.index();
        let disk = SlotState::Disk(DiskKey::new(5));
        table.publish(reserved, disk);

        let entry = AtomicBool::new(true);
        let storage_freed = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // The fault.
                loop {
                    match table.state(index) {
                        SlotState::Disk(key) => {
                            let Ok(claim) = table.claim(index, SlotState::Disk(key)) else {
                                continue;
                            };
                            if entry.swap(false, Ordering::AcqRel) {
                                storage_freed.fetch_add(1, Ordering::AcqRel);
                                table.free(claim);
                            } else {
                                // Reinstatement refused: the entry is gone.
                                table.publish(claim, SlotState::Disk(key));
                            }
                            return;
                        }
                        SlotState::Claimed => core::hint::spin_loop(),
                        _ => return,
                    }
                }
            });
            scope.spawn(|| {
                // The discard, then the orphan release.
                if !entry.swap(false, Ordering::AcqRel) {
                    return;
                }
                loop {
                    match table.state(index) {
                        SlotState::Disk(key) => {
                            if let Ok(claim) = table.claim(index, SlotState::Disk(key)) {
                                storage_freed.fetch_add(1, Ordering::AcqRel);
                                table.free(claim);
                                return;
                            }
                        }
                        SlotState::Claimed => core::hint::spin_loop(),
                        other => panic!("the discard found {other:?}"),
                    }
                }
            });
        });

        assert_eq!(
            storage_freed.load(Ordering::Acquire),
            1,
            "storage freed once"
        );
        assert_eq!(table.state(index), SlotState::Free { next: None });
        let reused = allocator.reserve(&table).expect("a slot");
        assert_eq!(reused.index(), index, "the slot is on the free chain once");
        let next = allocator.reserve(&table).expect("a slot");
        assert_ne!(next.index(), index, "and not twice");
    }
}
