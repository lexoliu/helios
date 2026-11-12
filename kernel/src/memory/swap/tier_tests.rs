//! The eviction ladder and the in-place fault path, driven against the same
//! fake address space and flaky disk as `service_tests`, with the compressed
//! store over the real user pool.
//!
//! Compressible pages are real bytes: slices of this crate's own source.
//! Incompressible pages are ChaCha20 output, the DRBG the kernel draws its
//! random bytes from. Zero pages are zeroes.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use std::sync::Barrier;

use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};

use super::service_tests::{FakePage, Harness, PAGE, block_on, poll, with_space};
use super::slot::StoredObject;
use super::test_pool::allocated_bytes;
use super::*;

/// Source text: far more than the pages the tests slice from it.
const TEXT: &[u8] = include_bytes!("mod.rs");

/// Whether a page of `bytes` passes the default compressibility gate.
fn compresses_under_the_gate(bytes: &[u8]) -> bool {
    let mut table = CompressTable::default();
    let mut block = vec![0_u8; lz4_flex::block::get_maximum_output_size(PAGE)];
    let len = compress_into_with_table(bytes, &mut block, &mut table).expect("compress");
    SwapPolicy::default().gate().accept(len).is_some()
}

/// The `number`th page of source text that the default gate accepts.
///
/// Prose and code compress to different degrees from page to page, so the
/// tests take the pages that compress well enough rather than fixed offsets
/// that an edit to the source would move.
fn text_page(number: usize) -> Vec<u8> {
    static PAGES: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    PAGES
        .get_or_init(|| {
            TEXT.chunks_exact(PAGE)
                .filter(|page| compresses_under_the_gate(page))
                .map(<[u8]>::to_vec)
                .collect()
        })
        .get(number)
        .unwrap_or_else(|| panic!("the source has no compressible page number {number}"))
        .clone()
}

fn random_page(seed: u64) -> Vec<u8> {
    let mut page = vec![0_u8; PAGE];
    ChaCha20Rng::seed_from_u64(seed).fill_bytes(&mut page);
    page
}

/// A page that compresses to a few hundred bytes: a short text repeated, with
/// the page number written into its first line so every page differs.
fn dense_page(number: usize) -> Vec<u8> {
    let line = format!("page {number} of the repeated line\n");
    line.bytes().cycle().take(PAGE).collect()
}

/// The object a slot holds, which must be in the store.
fn stored(harness: &Harness, addr: VirtAddr) -> StoredObject {
    let token = harness.index_of(addr).expect("the page is swapped out");
    match harness.slot(token) {
        SlotState::Stored(object) => object,
        other => panic!("slot {token:?} reads {other:?}, not a store object"),
    }
}

#[test]
fn the_source_has_the_compressible_pages_the_tests_use_and_random_bytes_are_not_among_them() {
    for number in 0..8 {
        assert!(compresses_under_the_gate(&text_page(number)));
    }
    assert!(!compresses_under_the_gate(&random_page(1)));
}

#[test]
fn a_zero_page_uses_no_store_frame_and_faults_back_as_zeros() {
    let mut harness = Harness::tiered();
    let addr = harness.resident_with(1, vec![0_u8; PAGE]);
    assert_eq!(harness.evict(addr), Ok(Placed::Out));
    let token = harness.index_of(addr).expect("swapped out");
    assert_eq!(harness.slot(token), SlotState::Zero);
    assert_eq!(
        harness.service.store.object_frames(),
        0,
        "a zero page takes no store frame"
    );
    assert_eq!(harness.backend.extents(), Vec::<Vec<u8>>::new());

    harness.fault(addr).expect("a zero page faults back");
    assert_eq!(harness.contents(addr), vec![0_u8; PAGE]);
    let totals = harness.handle.shared.counters.totals();
    assert_eq!((totals.pages_out_zero, totals.faults_zero), (1, 1));
    harness.finish();
}

#[test]
fn a_page_of_source_text_lands_in_the_store_and_round_trips_byte_exact() {
    let mut harness = Harness::tiered();
    let original = text_page(2);
    let addr = harness.resident_with(1, original.clone());
    assert_eq!(harness.evict(addr), Ok(Placed::Out));
    let object = stored(&harness, addr);
    assert!(
        object.len().get() < PAGE * 3 / 4,
        "the store holds the compressed block, not the page"
    );
    assert_eq!(harness.service.store.object_frames(), 1);
    assert_eq!(harness.backend.extents(), Vec::<Vec<u8>>::new());

    harness.fault(addr).expect("a stored page faults back");
    assert_eq!(harness.contents(addr), original);
    let totals = harness.handle.shared.counters.totals();
    assert_eq!((totals.pages_out_store, totals.faults_store), (1, 1));
    harness.finish();
}

#[test]
fn an_incompressible_page_goes_to_the_disk_raw() {
    let mut harness = Harness::tiered();
    let original = random_page(7);
    let addr = harness.resident_with(1, original.clone());
    assert_eq!(harness.evict(addr), Ok(Placed::Out));
    let token = harness.index_of(addr).expect("swapped out");
    assert!(matches!(harness.slot(token), SlotState::Disk(_)));
    assert_eq!(
        harness.backend.extents(),
        core::slice::from_ref(&original),
        "the disk holds the whole page"
    );
    assert_eq!(harness.service.store.object_frames(), 0);

    harness.fault(addr).expect("a disk page faults back");
    assert_eq!(harness.contents(addr), original);
    let totals = harness.handle.shared.counters.totals();
    assert_eq!(
        (
            totals.pages_out_disk_raw,
            totals.rejected_incompressible,
            totals.faults_disk
        ),
        (1, 1, 1)
    );
    harness.finish();
}

#[test]
fn with_no_disk_an_incompressible_page_is_put_back_and_stays_resident() {
    let mut harness = Harness::diskless();
    let original = random_page(8);
    let addr = harness.resident_with(1, original.clone());
    assert_eq!(harness.evict(addr), Ok(Placed::Kept));
    assert!(harness.is_resident(addr));
    assert_eq!(harness.contents(addr), original);
    assert_eq!(harness.index_of(addr), None);
    assert_eq!(harness.handle.shared.counters.totals().put_back, 1);

    // Nothing was kept in the store, and the slot went back for reuse.
    assert_eq!(harness.service.store.object_frames(), 0);
    let next = harness.resident_with(2, text_page(1));
    assert_eq!(harness.evict(next), Ok(Placed::Out));
    harness.finish_after_fault(next);
}

#[test]
fn a_put_back_the_address_space_refuses_parks_the_page() {
    let mut harness = Harness::diskless();
    let original = random_page(9);
    let addr = harness.resident_with(1, original.clone());
    with_space(|space| space.reinstatements_to_refuse = 1);
    assert_eq!(harness.evict(addr), Ok(Placed::Out));
    let token = harness.index_of(addr).expect("swapped out");
    assert!(
        matches!(harness.slot(token), SlotState::Parked(_)),
        "the page has nowhere else to be"
    );

    harness.fault(addr).expect("a parked page faults back");
    assert_eq!(harness.contents(addr), original);
    let totals = harness.handle.shared.counters.totals();
    assert_eq!((totals.pages_out_parked, totals.faults_parked), (1, 1));
    harness.finish();
}

#[test]
fn a_compressible_page_the_full_store_refuses_goes_to_the_disk_compressed() {
    let mut harness = Harness::tiered();
    harness
        .handle
        .set_policy(SwapPolicy {
            store_limit_percent: 0,
            ..SwapPolicy::default()
        })
        .expect("a limit of nothing is in range");
    let original = text_page(3);
    let addr = harness.resident_with(1, original.clone());
    assert_eq!(harness.evict(addr), Ok(Placed::Out));
    let token = harness.index_of(addr).expect("swapped out");
    let SlotState::Disk(key) = harness.slot(token) else {
        panic!("a page the store refuses goes to the disk");
    };
    let disk = harness.service.disk.as_ref().expect("a disk");
    assert!(
        matches!(disk.get(key).encoding, DiskEncoding::Lz4(_)),
        "the disk holds the compressed block"
    );
    let extents = harness.backend.extents();
    assert!(
        extents[0].len() < PAGE * 3 / 4,
        "the extent is the block, {} bytes",
        extents[0].len()
    );

    harness
        .fault(addr)
        .expect("a compressed disk page faults back");
    assert_eq!(harness.contents(addr), original);
    let totals = harness.handle.shared.counters.totals();
    assert_eq!(
        (
            totals.pages_out_disk_compressed,
            totals.rejected_full,
            totals.faults_disk
        ),
        (1, 1, 1)
    );
    harness.finish();
}

#[test]
fn a_failed_reinstatement_of_a_compressed_disk_page_rehomes_the_block() {
    let mut harness = Harness::tiered();
    harness
        .handle
        .set_policy(SwapPolicy {
            store_limit_percent: 0,
            ..SwapPolicy::default()
        })
        .expect("in range");
    let original = text_page(4);
    let addr = harness.resident_with(1, original.clone());
    harness.evict(addr).expect("evict");
    let extent = harness.backend.extents().remove(0);

    with_space(|space| space.reinstatements_to_refuse = 1);
    assert_eq!(harness.fault(addr), Err(SwapFaultError::OutOfFrames));
    assert_eq!(
        harness.backend.extents(),
        [extent],
        "the disk holds the same block again, under the same slot"
    );
    harness.fault(addr).expect("the next fault succeeds");
    assert_eq!(harness.contents(addr), original);
    harness.finish();
}

#[test]
fn a_disk_write_failure_after_a_detach_parks_the_page_and_nothing_is_lost() {
    let mut harness = Harness::tiered();
    let original = random_page(11);
    let addr = harness.resident_with(1, original.clone());
    harness.backend.refuse_writes(1);
    assert_eq!(harness.evict(addr), Ok(Placed::Out));
    let token = harness.index_of(addr).expect("swapped out");
    assert!(matches!(harness.slot(token), SlotState::Parked(_)));

    harness.fault(addr).expect("a parked page faults back");
    assert_eq!(harness.contents(addr), original);
    harness.finish();
}

#[test]
fn a_fault_that_cannot_get_a_frame_leaves_the_stored_page_as_it_was() {
    let mut harness = Harness::tiered();
    let original = text_page(5);
    let addr = harness.resident_with(1, original.clone());
    harness.evict(addr).expect("evict");
    let object = stored(&harness, addr);

    with_space(|space| space.reinstatements_to_refuse = 1);
    assert_eq!(harness.fault(addr), Err(SwapFaultError::OutOfFrames));
    assert_eq!(
        stored(&harness, addr),
        object,
        "the slot holds its object again"
    );

    harness.fault(addr).expect("the next fault succeeds");
    assert_eq!(harness.contents(addr), original);
    harness.finish();
}

#[test]
fn faults_from_two_threads_on_one_stored_page_both_succeed() {
    let mut harness = Harness::tiered();
    for round in 0..200 {
        let original = text_page(round % 4);
        let addr = harness.resident_with(1, original.clone());
        assert_eq!(harness.evict(addr), Ok(Placed::Out));

        let barrier = Barrier::new(2);
        let handle = &harness.handle;
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    barrier.wait();
                    block_on(handle.fault_in(addr)).expect("a fault on a stored page");
                });
            }
        });
        assert_eq!(
            harness.contents(addr),
            original,
            "round {round}: the page holds its own bytes"
        );
        let totals = harness.handle.shared.counters.totals();
        assert_eq!(
            totals.faults_store,
            round as u64 + 1,
            "round {round}: exactly one of the two faults decoded the page"
        );
        harness.service.store.drain(&harness.handle.shared.store);
    }
    harness.finish();
}

#[test]
fn a_fault_waits_while_another_path_holds_the_slot_and_then_completes() {
    let mut harness = Harness::tiered();
    let original = text_page(6);
    let addr = harness.resident_with(1, original.clone());
    harness.evict(addr).expect("evict");
    let token = harness.index_of(addr).expect("swapped out");
    let index = SlotIndex::from_token(token).expect("a valid token");
    let state = harness.slot(token);
    // Writeback or compaction would hold this claim.
    let claim = harness
        .handle
        .shared
        .slots
        .claim(index, state)
        .expect("a settled slot");

    let handle = harness.handle.clone();
    let mut fault = Box::pin(handle.fault_in(addr));
    assert!(
        poll(&mut fault).is_none(),
        "a fault on a claimed slot waits for it"
    );
    assert_eq!(harness.handle.shared.counters.totals().contended_waits, 1);
    assert!(
        poll(&mut fault).is_none(),
        "and keeps waiting without a signal"
    );

    harness.handle.shared.restore(claim, state);
    assert_eq!(poll(&mut fault), Some(Ok(())));
    assert_eq!(harness.contents(addr), original);
    harness.finish();
}

#[test]
fn a_fault_that_finds_the_slot_reserved_waits_for_the_placement() {
    let mut harness = Harness::tiered();
    let reserved = harness
        .service
        .slots
        .reserve(&harness.handle.shared.slots)
        .expect("a slot");
    let addr = VirtAddr::new(PAGE);
    with_space(|space| {
        space
            .pages
            .insert(addr.raw(), FakePage::Swapped(reserved.index().token()));
    });

    let handle = harness.handle.clone();
    let mut fault = Box::pin(handle.fault_in(addr));
    assert!(
        poll(&mut fault).is_none(),
        "the owner has not placed it yet"
    );

    // The owner places the page: a zero page, say.
    harness.handle.shared.restore(reserved, SlotState::Zero);
    assert_eq!(poll(&mut fault), Some(Ok(())));
    assert_eq!(harness.contents(addr), vec![0_u8; PAGE]);
    harness.finish();
}

#[test]
#[should_panic(expected = "instead of one page")]
fn a_store_object_that_fails_to_decode_panics_the_kernel() {
    let mut harness = Harness::tiered();
    let addr = harness.resident_with(1, text_page(7));
    harness.evict(addr).expect("evict");
    let object = stored(&harness, addr);

    // Memory corruption in the store: the block's bytes are gone.
    let frame = harness.handle.shared.store.frame_bytes(object.frame());
    // SAFETY: the object's granules lie inside its live frame, and the test
    // holds the only reference to the store.
    unsafe {
        core::ptr::write_bytes(
            frame.add(object.first().raw() * slot::GRANULE_BYTES + slot::OBJECT_HEADER_BYTES),
            0xff,
            object.len().get(),
        );
    }
    let _ = harness.fault(addr);
}

#[test]
fn teardown_frees_every_ram_tier_slot_and_returns_the_frames() {
    let mut harness = Harness::tiered();
    let zero = harness.resident_with(1, vec![0_u8; PAGE]);
    let text = harness.resident_with(2, text_page(1));
    let parked = harness.resident_with(3, random_page(12));
    harness.backend.refuse_writes(1);
    for addr in [zero, text, parked] {
        assert_eq!(harness.evict(addr), Ok(Placed::Out));
    }
    let tokens: Vec<SwapToken> = [zero, text, parked]
        .into_iter()
        .map(|addr| harness.index_of(addr).expect("swapped out"))
        .collect();
    assert!(matches!(harness.slot(tokens[0]), SlotState::Zero));
    assert!(matches!(harness.slot(tokens[1]), SlotState::Stored(_)));
    assert!(matches!(harness.slot(tokens[2]), SlotState::Parked(_)));

    for token in tokens {
        harness.teardown(token);
        assert!(
            matches!(harness.slot(token), SlotState::Free { .. }),
            "the slot is free after its address space let go of it"
        );
    }
    harness.finish();
}

#[test]
fn the_pool_pays_for_the_store_frames_and_compression_holds_fewer_than_it_frees() {
    let mut harness = Harness::tiered();
    let pages = 8;
    for number in 0..pages {
        let addr = harness.resident_with(number + 1, dense_page(number));
        assert_eq!(harness.evict(addr), Ok(Placed::Out));
    }
    let store = &harness.service.store;
    assert_eq!(
        allocated_bytes() - harness.baseline,
        store.frames() * PAGE,
        "every frame the store holds, the spare included, is allocated from the pool"
    );
    assert!(
        store.object_frames() < pages,
        "{pages} compressible pages occupy {} store frames",
        store.object_frames()
    );
    // Each detach frees one page frame and the store spends `object_frames`
    // plus the spare, so the pool gains `pages - frames`.
    assert!(pages > store.frames());
    for number in 0..pages {
        harness
            .fault(VirtAddr::new((number + 1) * PAGE))
            .expect("fault");
    }
    harness.finish();
}

#[test]
fn a_policy_outside_its_ranges_is_refused_and_leaves_the_settings_alone() {
    let harness = Harness::tiered();
    let before = harness.handle.policy();
    assert_eq!(before, SwapPolicy::default());

    for (policy, error) in [
        (
            SwapPolicy {
                store_limit_percent: 101,
                ..before
            },
            SwapPolicyError::StoreLimitPercent(101),
        ),
        (
            SwapPolicy {
                store_accept_percent: 101,
                ..before
            },
            SwapPolicyError::StoreAcceptPercent(101),
        ),
        (
            SwapPolicy {
                accept_granules: 0,
                ..before
            },
            SwapPolicyError::AcceptGranules(0),
        ),
        (
            SwapPolicy {
                accept_granules: 64,
                ..before
            },
            SwapPolicyError::AcceptGranules(64),
        ),
    ] {
        assert_eq!(harness.handle.set_policy(policy), Err(error));
        assert_eq!(harness.handle.policy(), before);
    }

    let changed = SwapPolicy {
        store_limit_percent: 35,
        writeback: false,
        ..before
    };
    harness.handle.set_policy(changed).expect("in range");
    assert_eq!(harness.handle.policy(), changed);
    harness.finish();
}

/// A harness that has faulted `addr` back, for a test that is done with it.
impl Harness {
    fn finish_after_fault(mut self, addr: VirtAddr) {
        self.fault(addr).expect("fault");
        self.finish();
    }
}

#[test]
fn the_owner_serves_a_queued_fault_whose_page_became_a_ram_page() {
    // A request queued for a page on the disk, whose slot changed hands
    // before the owner got to it, is resolved by the owner rather than
    // refused: the owner runs the same step as a faulting processor.
    let mut harness = Harness::tiered();
    let original = text_page(0);
    let addr = harness.resident_with(1, original.clone());
    harness.evict(addr).expect("evict");
    let handle = harness.handle.clone();
    // Queue the request directly, as a fault that saw the page on the disk
    // would have.
    let mut fault = Box::pin(handle.fault_via_owner(addr, 0));
    assert!(poll(&mut fault).is_none());
    harness.serve();
    assert_eq!(poll(&mut fault), Some(Ok(())));
    assert_eq!(harness.contents(addr), original);
    harness.finish();
}
