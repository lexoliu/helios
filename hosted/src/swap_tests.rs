//! Swap on the hosted backend, end to end against a real address space.
//!
//! These drive `HostedAddressSpace` and `FileSwapBackend` directly:
//! `mmap`/`mprotect` really change, the swap file really holds the
//! bytes, and a page that comes back is checked by reading it. What they
//! deliberately do not do is measure anything — `hosted` sits on the
//! host page cache and is not evidence about swap on a real target.

use std::path::Path;

use helios_hal::vmm::{
    AddressSpace, PageAge, PageFlags, SwapBackend, SwapToken, Translation, VirtAddr, VirtRange,
};
use helios_kernel::{MemoryOwner, enter_user_memory_owner};

use crate::swap::FileSwapBackend;
use crate::vmm::HostedAddressSpace;

const PAGE: usize = 4096;

fn block_on<F: Future>(future: F) -> F::Output {
    futures_lite::future::block_on(future)
}

fn swap_file(name: &str) -> FileSwapBackend {
    let path = std::env::temp_dir().join(format!(
        "helios-hosted-swap-{}-{name}.bin",
        std::process::id()
    ));
    FileSwapBackend::create(Path::new(&path), 1 << 20).expect("swap file")
}

fn token(raw: u32) -> SwapToken {
    SwapToken::new(raw).expect("non-zero token")
}

fn fill(range: VirtRange, seed: u8) {
    unsafe {
        let bytes = range.start.raw() as *mut u8;
        for index in 0..range.byte_len {
            bytes
                .add(index)
                .write_volatile(seed.wrapping_add(index as u8));
        }
    }
}

fn check(range: VirtRange, seed: u8) {
    unsafe {
        let bytes = range.start.raw() as *const u8;
        for index in 0..range.byte_len {
            assert_eq!(
                bytes.add(index).read_volatile(),
                seed.wrapping_add(index as u8),
                "byte {index} of {:#x} came back wrong",
                range.start.raw()
            );
        }
    }
}

/// One processor, one owner scope: the whole point of the scope is that
/// what it commits is attributable afterwards.
fn commit_owned(address_space: &HostedAddressSpace, range: VirtRange, owner: MemoryOwner) {
    let _scope = enter_user_memory_owner(helios_hal::cpu::ProcessorId::new(0), owner);
    address_space
        .commit(range, PageFlags::READ | PageFlags::WRITE)
        .expect("commit");
}

#[test]
fn a_swapped_page_comes_back_with_its_bytes_and_its_flags() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let backend = swap_file("round-trip");
    let range = address_space.reserve(PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(1));
    fill(range, 0x40);

    let mut page = vec![0_u8; PAGE];
    let flags = address_space
        .swap_out_page(range.start, token(1), &mut page)
        .expect("swap out");
    assert_eq!(flags, PageFlags::READ | PageFlags::WRITE);
    assert_eq!(
        address_space.translate(range.start),
        Translation::Reserved,
        "a swapped page must no longer read as committed"
    );
    assert_eq!(address_space.swapped_token(range.start), Some(token(1)));

    let stored = block_on(backend.swap_out(&page)).expect("backend swap out");
    let mut restored = vec![0_u8; PAGE];
    block_on(backend.swap_in(stored, &mut restored)).expect("backend swap in");

    let recovered = address_space
        .swap_in_page(range.start, &restored)
        .expect("swap in");
    assert_eq!(recovered, token(1));
    check(range, 0x40);
    match address_space.translate(range.start) {
        Translation::Committed { flags, .. } => {
            assert_eq!(flags, PageFlags::READ | PageFlags::WRITE)
        }
        other => panic!("expected the page back committed, got {other:?}"),
    }
    address_space.release(range).expect("release");
}

#[test]
fn a_swapped_page_is_still_writable_after_it_comes_back() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(2));
    fill(range, 0x11);

    let mut page = vec![0_u8; PAGE];
    address_space
        .swap_out_page(range.start, token(2), &mut page)
        .expect("swap out");
    address_space
        .swap_in_page(range.start, &page)
        .expect("swap in");

    fill(range, 0x99);
    check(range, 0x99);
    address_space.release(range).expect("release");
}

#[test]
fn only_the_owner_s_pages_are_offered_to_a_pass() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let mine = address_space.reserve(2 * PAGE).expect("reserve");
    let theirs = address_space.reserve(PAGE).expect("reserve");
    commit_owned(&address_space, mine, MemoryOwner::new(7));
    commit_owned(&address_space, theirs, MemoryOwner::new(8));

    assert_eq!(address_space.owned_resident_bytes(7), 2 * PAGE as u64);
    assert_eq!(address_space.owned_resident_bytes(8), PAGE as u64);

    let mut seen = Vec::new();
    address_space.scan_committed_pages(7, |addr, _flags, _age| {
        seen.push(addr);
        true
    });
    assert_eq!(
        seen,
        [mine.start, VirtAddr::new(mine.start.raw() + PAGE)],
        "a pass must be offered exactly the owner's pages"
    );

    address_space.release(mine).expect("release");
    address_space.release(theirs).expect("release");
}

#[test]
fn a_pass_that_says_stop_stops_the_scan() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(4 * PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(9));

    let mut seen = 0_usize;
    address_space.scan_committed_pages(9, |_addr, _flags, _age| {
        seen += 1;
        seen < 2
    });
    assert_eq!(seen, 2, "the scan must stop when the pass says so");
    address_space.release(range).expect("release");
}

/// The host kernel does not show a process its access flags, so hosted
/// reports every page hot and an aging pass frees nothing here. That is
/// the honest answer, and it is what makes the OOM killer the next step
/// on this backend.
#[test]
fn hosted_reports_every_page_hot_because_it_cannot_measure_age() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(2 * PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(11));

    let mut ages = Vec::new();
    address_space.scan_committed_pages(11, |_addr, _flags, age| {
        ages.push(age);
        true
    });
    assert_eq!(ages, [PageAge::Hot, PageAge::Hot]);
    address_space.release(range).expect("release");
}

#[test]
fn releasing_a_reservation_surrenders_the_tokens_it_still_held() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(2 * PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(3));

    let mut page = vec![0_u8; PAGE];
    address_space
        .swap_out_page(range.start, token(21), &mut page)
        .expect("first page out");
    address_space
        .swap_out_page(
            VirtAddr::new(range.start.raw() + PAGE),
            token(22),
            &mut page,
        )
        .expect("second page out");

    address_space.release(range).expect("release");

    let mut orphaned = Vec::new();
    let count = address_space.drain_orphaned_swap_tokens(|token| orphaned.push(token));
    assert_eq!(count, 2);
    orphaned.sort();
    assert_eq!(
        orphaned,
        [token(21), token(22)],
        "an instance's death must surrender every token it held"
    );
    assert_eq!(
        address_space.drain_orphaned_swap_tokens(|_| {}),
        0,
        "draining twice must not hand the same token out again"
    );
}

#[test]
fn decommitting_a_range_surrenders_the_tokens_inside_it() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(2 * PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(4));
    let mut page = vec![0_u8; PAGE];
    address_space
        .swap_out_page(range.start, token(31), &mut page)
        .expect("page out");

    let live = VirtRange::new(VirtAddr::new(range.start.raw() + PAGE), PAGE);
    address_space
        .decommit(live)
        .expect("decommit the live page");
    assert_eq!(
        address_space.drain_orphaned_swap_tokens(|_| {}),
        0,
        "decommitting elsewhere must not disturb a swapped page"
    );

    address_space.release(range).expect("release");
    let mut orphaned = Vec::new();
    address_space.drain_orphaned_swap_tokens(|token| orphaned.push(token));
    assert_eq!(orphaned, [token(31)]);
}

#[test]
fn committing_over_a_swapped_page_surrenders_its_token() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(5));
    let mut page = vec![0_u8; PAGE];
    address_space
        .swap_out_page(range.start, token(41), &mut page)
        .expect("page out");

    commit_owned(&address_space, range, MemoryOwner::new(5));
    let mut orphaned = Vec::new();
    address_space.drain_orphaned_swap_tokens(|token| orphaned.push(token));
    assert_eq!(
        orphaned,
        [token(41)],
        "fresh anonymous memory over a swapped page must release its extent"
    );
    address_space.release(range).expect("release");
}

#[test]
fn swapping_a_page_that_is_not_committed_is_refused() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(PAGE).expect("reserve");
    let mut page = vec![0_u8; PAGE];
    assert!(
        address_space
            .swap_out_page(range.start, token(51), &mut page)
            .is_err(),
        "a reserved-but-uncommitted page has nothing to write out"
    );
    address_space.release(range).expect("release");
}

#[test]
fn a_page_buffer_of_the_wrong_size_is_refused() {
    helios_kernel::configure_user_memory_owner_processors(1);
    let address_space = HostedAddressSpace::new();
    let range = address_space.reserve(PAGE).expect("reserve");
    commit_owned(&address_space, range, MemoryOwner::new(6));
    let mut short = vec![0_u8; 128];
    assert!(
        address_space
            .swap_out_page(range.start, token(61), &mut short)
            .is_err()
    );
    address_space.release(range).expect("release");
}
