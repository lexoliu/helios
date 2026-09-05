//! What mapping user virtual address space costs the *kernel* heap.
//!
//! Kernel memory and user memory are separate ownership domains
//! (AGENTS §3), but they are not disjoint at the hardware level: the
//! pages behind a wasm linear memory come from the user pool, while the
//! structures that address and describe those pages do not. Every
//! bare-metal backend allocates its page tables with `alloc_zeroed` on
//! the kernel heap (`ensure_table` in `aarch64/src/vmm.rs`,
//! `x86/src/vmm.rs` and `riscv/src/vmm.rs`), and
//! [`ReservationTracker`](super::ReservationTracker) keeps its
//! committed-region records in kernel-heap `Vec`s.
//!
//! So a user-pool grow does have a kernel-heap cost. It is simply
//! nowhere near the size of the grow: one page table addresses 512
//! pages, so the kernel side of a grow is roughly a five-hundredth of
//! it. Charging the growth itself against the kernel heap — which the
//! grow admission path did — refuses grows the kernel heap was never
//! asked to fund.
//!
//! One model, two readers: the admission check in the Wasmtime
//! `ResourceLimiter` and the per-instance kernel-heap footprint the OOM
//! killer ranks on both call [`user_mapping_kernel_heap_bytes`], so the
//! bytes an instance is charged for its memory are the same bytes its
//! grow was admitted against.

use helios_hal::pmm::PhysFrame;

use super::reservations::CommittedRegion;

/// Descriptors in one page-table page.
///
/// Every architecture helios targets addresses user memory through
/// page-sized tables of 8-byte descriptors: aarch64 on the 4 KiB
/// granule, x86-64's 4-level paging, and riscv sv39/sv48.
const PAGE_TABLE_ENTRIES: usize = PhysFrame::SIZE / size_of::<u64>();

/// User virtual bytes one leaf page-table page maps.
const LEAF_TABLE_SPAN_BYTES: usize = PAGE_TABLE_ENTRIES * PhysFrame::SIZE;

/// Page-table levels a backend allocates below the root it booted with.
///
/// aarch64 walks the L0 in `TTBR1` down through L1/L2/L3, x86-64 the
/// PML4 in `CR3` down through PDPT/PD/PT, and riscv sv48 the same depth
/// with sv39 one level shallower. Three allocated levels is the deepest
/// of them, and an admission check wants the deepest.
const ALLOCATED_TABLE_LEVELS: u32 = 3;

/// Kernel heap one committed-region record costs the reservation
/// tracker: the record itself, plus its own size again for the headroom
/// the `Vec` it lands in reserves by doubling.
const RESERVATION_RECORD_BYTES: usize = 2 * size_of::<CommittedRegion>();

/// Committed-region records one mapping change can add.
///
/// Making a range accessible records the fresh region, and
/// re-protecting a range that falls inside an existing region splits
/// that region into the two fragments around it.
const RECORDS_PER_MAPPING: usize = 3;

/// Kernel heap that mapping `mapped_bytes` of user virtual address
/// space costs.
///
/// An upper bound rather than a measurement: the range is assumed to
/// straddle a table boundary at every level (the `+ 1` per level) and
/// to need every record one accessibility change can add. A backend
/// that maps user memory through the host's own page tables —
/// `hosted/` — pays only the tracker records, and over-charging there
/// is the safe direction.
pub const fn user_mapping_kernel_heap_bytes(mapped_bytes: usize) -> usize {
    if mapped_bytes == 0 {
        return 0;
    }

    let mut tables = 0_usize;
    let mut span = LEAF_TABLE_SPAN_BYTES;
    let mut level = 0;
    while level < ALLOCATED_TABLE_LEVELS {
        tables = tables.saturating_add(mapped_bytes / span + 1);
        span = span.saturating_mul(PAGE_TABLE_ENTRIES);
        level += 1;
    }

    tables
        .saturating_mul(PhysFrame::SIZE)
        .saturating_add(RECORDS_PER_MAPPING.saturating_mul(RESERVATION_RECORD_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_mapped_costs_nothing() {
        assert_eq!(user_mapping_kernel_heap_bytes(0), 0);
    }

    /// The defect in #120: the grow admission path charged the kernel
    /// heap the whole growth. A page table addresses 512 pages, so the
    /// kernel side of a grow is smaller than the grow by that factor —
    /// two orders of magnitude of headroom the check was throwing away.
    #[test]
    fn a_grow_costs_the_kernel_heap_orders_of_magnitude_less_than_the_grow() {
        const GROWTH: usize = 512 * 1024 * 1024;

        let cost = user_mapping_kernel_heap_bytes(GROWTH);
        assert!(
            cost < GROWTH / 100,
            "a {GROWTH}-byte grow must not cost the kernel heap {cost} bytes"
        );
        // 512 MiB needs 256 leaf tables; the levels above and the
        // boundary tables are what puts it just over.
        assert!(cost >= 256 * PhysFrame::SIZE, "{cost}");
    }

    #[test]
    fn the_cost_never_shrinks_as_the_mapping_grows() {
        let mut previous = 0;
        for megabytes in [1_usize, 2, 4, 16, 64, 256, 1024, 4096] {
            let cost = user_mapping_kernel_heap_bytes(megabytes * 1024 * 1024);
            assert!(cost >= previous, "{megabytes} MiB regressed to {cost}");
            previous = cost;
        }
    }

    /// A mapping the size of the whole address space must not wrap the
    /// cost into something small enough to admit. One leaf table per
    /// [`LEAF_TABLE_SPAN_BYTES`] is the floor, and no kernel heap is
    /// ever that large.
    #[test]
    fn an_absurd_mapping_stays_absurd_instead_of_wrapping() {
        let cost = user_mapping_kernel_heap_bytes(usize::MAX);
        assert!(cost >= usize::MAX / PAGE_TABLE_ENTRIES, "{cost}");
    }
}
