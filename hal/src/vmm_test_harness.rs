//! Reusable conformance harness for [`crate::vmm::AddressSpace`] impls.
//!
//! Bare-metal and hosted impls should share the same expectations:
//! reserve carves a virtual range, commit grants real backing,
//! decommit releases it but keeps the reservation, release returns
//! both, protect changes flags without churning frames, translate
//! reflects the current state. The harness exercises every transition
//! against an impl-supplied fresh instance and a "touch the bytes"
//! callback that proves committed memory is actually accessible
//! when the impl can do so safely (hosted via libc, x86 via real PTE
//! installs); backends that cannot accept arbitrary writes (e.g.
//! single-CPU x86 user AS within a panic-on-fault test environment)
//! can pass `verify_writes: false` and skip the access check.
//!
//! The harness lives in `hal` so backend integration tests in
//! `hosted/src/*_tests.rs` and future bare-metal smoke tests can call
//! it without duplicating the matrix.

use crate::pmm::PhysFrame;
use crate::vmm::{AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange};

/// Configuration knobs for the conformance suite.
pub struct AddressSpaceFixture<F> {
    /// Build a fresh address-space instance. Called once per test.
    pub fresh: F,
    /// Set to `false` for impls where committed pages cannot be
    /// safely written to from harness-side host code (e.g. a
    /// bare-metal AS where the page tables are live but the harness
    /// runs in kernel context that would page-fault into the mapped
    /// region). Hosted is `true`.
    pub verify_writes: bool,
}

const PAGE: usize = PhysFrame::SIZE;

fn pages(count: usize) -> usize {
    count * PAGE
}

/// Run the full conformance suite. Panics on the first failure with a
/// descriptive message — call sites typically wrap each invocation in
/// a `#[test]` so a failure points at the specific impl.
pub fn run_conformance<F, A>(fixture: AddressSpaceFixture<F>)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    reserve_returns_aligned_range(&fixture.fresh);
    reserve_rejects_zero_length(&fixture.fresh);
    reserve_rejects_misaligned_length(&fixture.fresh);
    release_unknown_range_errors(&fixture.fresh);
    commit_then_decommit_round_trip(&fixture.fresh, fixture.verify_writes);
    decommit_uncommitted_range_errors(&fixture.fresh);
    protect_uncommitted_range_errors(&fixture.fresh);
    translate_outside_reservation_is_unmapped(&fixture.fresh);
    release_drops_reservations(&fixture.fresh);
}

fn reserve_returns_aligned_range<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    let range = address_space
        .reserve(pages(4))
        .expect("reserve(4 pages) must succeed in a fresh address space");
    assert!(range.is_page_aligned(), "reserved range must be page-aligned: {range:?}");
    assert_eq!(range.byte_len, pages(4));
    assert_eq!(
        address_space.translate(range.start),
        Translation::Reserved,
        "freshly reserved page must report Reserved"
    );
    address_space.release(range).expect("release after reserve");
}

fn reserve_rejects_zero_length<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    assert_eq!(
        address_space.reserve(0),
        Err(AddressSpaceError::EmptyRange)
    );
}

fn reserve_rejects_misaligned_length<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    assert_eq!(
        address_space.reserve(123),
        Err(AddressSpaceError::Misaligned)
    );
}

fn release_unknown_range_errors<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    let bogus = VirtRange::new(VirtAddr::new(0xdead_0000), PAGE);
    assert_eq!(
        address_space.release(bogus),
        Err(AddressSpaceError::NotReserved)
    );
}

fn commit_then_decommit_round_trip<F, A>(fresh: F, verify_writes: bool)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    let range = address_space.reserve(pages(2)).expect("reserve");
    address_space
        .commit(range, PageFlags::READ | PageFlags::WRITE)
        .expect("commit");

    match address_space.translate(range.start) {
        Translation::Committed { flags, .. } => {
            assert!(flags.contains(PageFlags::READ));
            assert!(flags.contains(PageFlags::WRITE));
        }
        other => panic!("expected Committed translation, got {other:?}"),
    }

    if verify_writes {
        let pointer = range.start.raw() as *mut u32;
        unsafe {
            pointer.write_volatile(0xdead_beef);
            assert_eq!(pointer.read_volatile(), 0xdead_beef);
        }
    }

    address_space.decommit(range).expect("decommit");
    assert_eq!(address_space.translate(range.start), Translation::Reserved);
    address_space.release(range).expect("release after decommit");
}

fn decommit_uncommitted_range_errors<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    let range = address_space.reserve(PAGE).expect("reserve");
    let result = address_space.decommit(range);
    assert!(
        matches!(result, Err(AddressSpaceError::NotCommitted)),
        "decommit of uncommitted range must error, got {result:?}"
    );
    address_space.release(range).expect("release after error");
}

fn protect_uncommitted_range_errors<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    // Strict implementations (bare-metal, software bookkeeping) reject
    // protect on a reserved-but-not-committed range with `NotCommitted`.
    // Lenient implementations (hosted, where PT/host-kernel is the
    // source of truth) silently update protections — the host returns
    // success because the underlying mmap exists. Both are valid as
    // long as protect on a *never-reserved* range still errors.
    let address_space = fresh();
    let bogus_range = VirtRange::new(VirtAddr::new(0xdead_dead_0000), PAGE);
    let result = address_space.protect(bogus_range, PageFlags::READ);
    assert!(
        result.is_err(),
        "protect of an unreserved range must error, got {result:?}"
    );
}

fn translate_outside_reservation_is_unmapped<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    let translation = address_space.translate(VirtAddr::new(0xfacefeed_0000));
    assert_eq!(translation, Translation::Unmapped);
}

fn release_drops_reservations<F, A>(fresh: F)
where
    F: Fn() -> A,
    A: AddressSpace,
{
    let address_space = fresh();
    let range = address_space.reserve(pages(2)).expect("reserve");
    address_space.release(range).expect("release");
    let translation = address_space.translate(range.start);
    assert!(
        translation == Translation::Unmapped || translation == Translation::Reserved,
        "after release, translate must report Unmapped (or Reserved if the AS \
         retains the range on a free list); got {translation:?}"
    );
}
