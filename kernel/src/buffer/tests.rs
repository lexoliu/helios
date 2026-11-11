//! Buffers and views against the recording address space.
//!
//! The address space is the recording surface `device::platform`'s test
//! hooks install, so a test sees exactly which spans were substituted,
//! restored and withdrawn, and when a run was taken and given back.

use alloc::vec::Vec;

use helios_hal::device::{DeviceRegion, DeviceRegionAttributes};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{VirtAddr, VirtRange};

use crate::device::test_hooks::{self, MappingChange};

use super::{
    Buffer, BufferError, GuestMemory, MAX_VIEWS, Span, ViewError, ViewTable, WASM_PAGE_BYTES,
};

/// Where the tests pretend the instance's memory sits.
const MEMORY_BASE: usize = 0x1_0000_0000;

/// An instance's memory `pages` wasm pages long.
fn memory(pages: u64) -> GuestMemory {
    GuestMemory::new(VirtAddr::new(MEMORY_BASE), pages * WASM_PAGE_BYTES)
}

/// `pages` wasm pages of the memory starting at page `first`.
const fn pages(first: u64, pages: u64) -> Span {
    Span::new(first * WASM_PAGE_BYTES, pages * WASM_PAGE_BYTES)
}

/// The address-space range a view of `bytes` at `span` substitutes.
fn substituted(span: Span, bytes: u64) -> VirtRange {
    VirtRange::new(
        VirtAddr::new(MEMORY_BASE + span.offset as usize),
        bytes as usize,
    )
}

fn pinned(bytes: u64) -> Buffer {
    Buffer::pinned(bytes).expect("the test pool has room")
}

/// The changes recorded on this thread since `before` of them.
fn changes_since(before: usize) -> Vec<MappingChange> {
    test_hooks::changes()[before..].to_vec()
}

/// A buffer is whole granules, and a buffer of nothing is refused
/// rather than described.
#[test]
fn a_buffer_is_whole_granules_and_never_empty() {
    test_hooks::install();

    assert_eq!(Buffer::pinned(0).err(), Some(BufferError::Empty));
    let buffer = pinned(1);
    assert_eq!(buffer.bytes(), PhysFrame::SIZE as u64);
    assert_eq!(buffer.frames().frame_count, 1);
    assert!(buffer.same_run(&buffer.clone()));
    assert!(!buffer.same_run(&pinned(1)));
}

/// Everything the interface's span rule says, in the order a guest
/// would trip over it.
#[test]
fn a_span_is_a_run_of_whole_wasm_pages_inside_the_memory() {
    test_hooks::install();
    let table = ViewTable::new();
    let memory = memory(4);

    assert_eq!(
        table.validate(memory, pages(1, 1), WASM_PAGE_BYTES),
        Ok(VirtAddr::new(MEMORY_BASE + WASM_PAGE_BYTES as usize))
    );
    assert_eq!(
        table.validate(memory, Span::new(0, 0), 1),
        Err(ViewError::Misaligned),
        "an empty span"
    );
    assert_eq!(
        table.validate(memory, Span::new(4096, WASM_PAGE_BYTES), 1),
        Err(ViewError::Misaligned),
        "an offset inside a wasm page"
    );
    assert_eq!(
        table.validate(memory, Span::new(0, 4096), 1),
        Err(ViewError::Misaligned),
        "a length that is not whole wasm pages"
    );
    assert_eq!(
        table.validate(memory, pages(3, 2), 2 * WASM_PAGE_BYTES),
        Err(ViewError::OutOfBounds {
            memory_bytes: 4 * WASM_PAGE_BYTES
        }),
        "a span past the memory's current size"
    );
    assert_eq!(
        table.validate(
            memory,
            Span::new(u64::MAX - WASM_PAGE_BYTES + 1, WASM_PAGE_BYTES),
            1
        ),
        Err(ViewError::OutOfBounds {
            memory_bytes: 4 * WASM_PAGE_BYTES
        }),
        "a span whose end overflows the range"
    );
    assert_eq!(
        table.validate(memory, pages(0, 2), WASM_PAGE_BYTES),
        Err(ViewError::WrongLength {
            expected: WASM_PAGE_BYTES,
            given: 2 * WASM_PAGE_BYTES
        }),
        "a span longer than what is shown"
    );
    assert_eq!(
        table.validate(memory, pages(0, 1), WASM_PAGE_BYTES + 1),
        Err(ViewError::WrongLength {
            expected: 2 * WASM_PAGE_BYTES,
            given: WASM_PAGE_BYTES
        }),
        "a span shorter than what is shown"
    );
}

/// Which refusals are the span's own fault and which are the store's or
/// the machine's, as the interfaces' `bad-span` answers them.
#[test]
fn only_a_refusal_of_the_span_itself_is_a_bad_span() {
    assert!(ViewError::NoUnsharedMemory.is_bad_span());
    assert!(ViewError::Misaligned.is_bad_span());
    assert!(ViewError::OutOfBounds { memory_bytes: 0 }.is_bad_span());
    assert!(
        ViewError::WrongLength {
            expected: 0,
            given: 0
        }
        .is_bad_span()
    );
    assert!(ViewError::Overlap.is_bad_span());
    assert!(!ViewError::TooManyViews.is_bad_span());
    assert!(!ViewError::NotShown.is_bad_span());
    assert!(!ViewError::UnalignedRegion.is_bad_span());
    assert!(!ViewError::OutOfMemory { bytes: 0 }.is_bad_span());
}

/// Showing a buffer substitutes only the buffer's own bytes, and a
/// second span that shares a byte with a live view is refused.
#[test]
fn a_shown_buffer_substitutes_its_own_bytes_and_claims_its_span() {
    test_hooks::install();
    let mut table = ViewTable::new();
    let memory = memory(4);
    let buffer = pinned(4096);
    let before = test_hooks::changes().len();

    table
        .show_buffer(memory, pages(1, 1), &buffer, None)
        .expect("the span is free");

    assert_eq!(
        changes_since(before),
        [MappingChange::SubstituteMemory(
            substituted(pages(1, 1), buffer.bytes()),
            buffer.physical()
        )]
    );
    assert_eq!(
        table.validate(memory, pages(0, 2), 2 * WASM_PAGE_BYTES),
        Err(ViewError::Overlap)
    );
    assert_eq!(
        table.show_buffer(memory, pages(1, 1), &buffer, None),
        Err(ViewError::Overlap),
        "a span already shown"
    );
    table
        .show_buffer(memory, pages(2, 1), &buffer, None)
        .expect("the neighbouring span is free, and one buffer may be shown twice");
    assert_eq!(table.len(), 2);
}

/// Ending a view puts zeroed memory back and lets go of the view's
/// reference, so the run goes back to the pool once its last holder has
/// let go.
#[test]
fn ending_a_view_restores_the_span_and_frees_a_run_nothing_else_holds() {
    test_hooks::install();
    let mut table = ViewTable::new();
    let memory = memory(4);
    let runs = test_hooks::live_runs();
    let buffer = pinned(4096);
    let physical = buffer.physical();
    let view = table
        .show_buffer(memory, pages(0, 1), &buffer, None)
        .expect("the span is free");

    drop(buffer);
    assert_eq!(
        test_hooks::live_runs(),
        runs + 1,
        "the view still holds the run"
    );
    let before = test_hooks::changes().len();

    table.end(view).expect("the pool can refill the span");

    assert_eq!(
        changes_since(before),
        [
            MappingChange::Restore(substituted(pages(0, 1), physical.bytes)),
            MappingChange::Freed(physical)
        ]
    );
    assert_eq!(test_hooks::live_runs(), runs);
    assert!(table.is_empty());
    assert!(!table.is_shown(view));
    assert_eq!(table.end(view), Err(ViewError::NotShown));
}

/// A device region is shown with no run behind it, and a region that
/// does not start and end on a frame boundary is refused before the
/// address space is asked.
#[test]
fn a_device_region_is_shown_without_a_run() {
    test_hooks::install();
    let mut table = ViewTable::new();
    let memory = memory(4);
    let runs = test_hooks::live_runs();
    let region = DeviceRegion::new(
        PhysicalRange::new(0x1000_0000, 2 * PhysFrame::SIZE as u64),
        DeviceRegionAttributes::REGISTERS,
    );
    let ragged = DeviceRegion::new(
        PhysicalRange::new(0x1000_0010, PhysFrame::SIZE as u64),
        DeviceRegionAttributes::REGISTERS,
    );
    let before = test_hooks::changes().len();

    assert_eq!(
        table.show_device(memory, pages(0, 1), ragged, None),
        Err(ViewError::UnalignedRegion)
    );
    let view = table
        .show_device(memory, pages(0, 1), region, None)
        .expect("the span is free");
    table.end(view).expect("the pool can refill the span");

    let shown = substituted(pages(0, 1), region.physical.bytes);
    assert_eq!(
        changes_since(before),
        [
            MappingChange::SubstituteDevice(shown),
            MappingChange::Restore(shown)
        ]
    );
    assert_eq!(test_hooks::live_runs(), runs);
}

/// A group ends together and leaves every other view alone.
#[test]
fn ending_a_group_ends_only_its_own_views() {
    test_hooks::install();
    let mut table = ViewTable::new();
    let memory = memory(4);
    let buffer = pinned(4096);
    let group = table.group();
    let other = table.group();
    assert_ne!(group, other);
    table
        .show_buffer(memory, pages(0, 1), &buffer, Some(group))
        .expect("the span is free");
    table
        .show_buffer(memory, pages(1, 1), &buffer, Some(group))
        .expect("the span is free");
    let outside = table
        .show_buffer(memory, pages(2, 1), &buffer, Some(other))
        .expect("the span is free");
    let loose = table
        .show_buffer(memory, pages(3, 1), &buffer, None)
        .expect("the span is free");
    let before = test_hooks::changes().len();

    table
        .end_group(group)
        .expect("the pool can refill the spans");

    let mut restored: Vec<_> = changes_since(before)
        .into_iter()
        .map(|change| match change {
            MappingChange::Restore(range) => range.start.raw(),
            unexpected => panic!("ending a group asked for {unexpected:?}"),
        })
        .collect();
    restored.sort_unstable();
    assert_eq!(
        restored,
        [
            substituted(pages(0, 1), 0).start.raw(),
            substituted(pages(1, 1), 0).start.raw()
        ]
    );
    assert!(table.is_shown(outside));
    assert!(table.is_shown(loose));
    assert_eq!(table.len(), 2);
}

/// A dying store's table withdraws every view it still carries, which
/// allocates nothing, and lets go of every run only its views held.
#[test]
fn dropping_the_table_withdraws_every_view_and_frees_what_only_it_held() {
    test_hooks::install();
    let mut table = ViewTable::new();
    let memory = memory(4);
    let runs = test_hooks::live_runs();
    let kept = pinned(4096);
    let dropped = pinned(4096);
    table
        .show_buffer(memory, pages(0, 1), &kept, None)
        .expect("the span is free");
    table
        .show_buffer(memory, pages(1, 1), &dropped, None)
        .expect("the span is free");
    let dropped_run = dropped.physical();
    drop(dropped);
    let before = test_hooks::changes().len();

    drop(table);

    let changes = changes_since(before);
    let withdrawn = changes
        .iter()
        .filter(|change| matches!(change, MappingChange::Withdraw(_)))
        .count();
    assert_eq!(withdrawn, 2);
    assert!(
        !changes
            .iter()
            .any(|change| matches!(change, MappingChange::Restore(_))),
        "a dying store puts nothing back"
    );
    assert!(changes.contains(&MappingChange::Freed(dropped_run)));
    assert_eq!(
        test_hooks::live_runs(),
        runs + 1,
        "a run another holder keeps outlives the views"
    );
    drop(kept);
    assert_eq!(test_hooks::live_runs(), runs);
}

/// The table's size is the kernel's choice, not the guest's: past
/// [`MAX_VIEWS`] a span is refused before anything is substituted.
#[test]
fn a_store_holds_at_most_max_views() {
    test_hooks::install();
    let mut table = ViewTable::new();
    let views = MAX_VIEWS as u64;
    let memory = memory(views + 1);
    let buffer = pinned(4096);
    for page in 0..views {
        table
            .show_buffer(memory, pages(page, 1), &buffer, None)
            .expect("below the bound");
    }
    let before = test_hooks::changes().len();

    assert_eq!(
        table.show_buffer(memory, pages(views, 1), &buffer, None),
        Err(ViewError::TooManyViews)
    );
    assert!(changes_since(before).is_empty());
    assert_eq!(table.len(), MAX_VIEWS);
}
