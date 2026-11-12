//! Swap's counters, one block per processor.
//!
//! # Concurrency contract
//!
//! - **Writing** runs on any processor and touches only that processor's own
//!   block ([`Counters::here`], indexed by the executing processor), with
//!   relaxed `fetch_add`s. The owner task writes the block of whichever
//!   processor it is on, as every other task does. Nothing is held across an
//!   await, so a processor that goes offline leaves nothing to hand over: its
//!   block stays and keeps being summed.
//! - **Reading** ([`Counters::totals`]) runs on any task and sums every block
//!   with relaxed loads. A total is exact once the writers it covers have
//!   stopped, and approximate while they run.
//! - Gauges are signed per-processor deltas: a page can go out on one
//!   processor and come back on another, so a single block's value can be
//!   negative while the sum is exact.
//!
//! Each block is a cache line (or more) of its own, so processors never share
//! a line by writing their counters.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;

/// Where a page came back from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FaultSource {
    Zero,
    Store,
    Parked,
    Disk,
}

/// Faults resolved from one source and the time guests waited for them.
#[derive(Default)]
struct FaultCounter {
    count: AtomicU64,
    wait_nanos: AtomicU64,
}

/// One processor's counters.
#[derive(Default)]
pub(super) struct CounterBlock {
    pages_out_zero: AtomicU64,
    pages_out_store: AtomicU64,
    pages_out_disk_raw: AtomicU64,
    pages_out_disk_compressed: AtomicU64,
    pages_out_parked: AtomicU64,
    put_back: AtomicU64,
    rejected_incompressible: AtomicU64,
    rejected_full: AtomicU64,
    contended_waits: AtomicU64,
    /// Pages on the disk: a gauge, as a delta.
    disk_pages: AtomicI64,
    faults: [FaultCounter; 4],
}

/// Where an evicted page went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Placement {
    Zero,
    Store,
    DiskRaw,
    DiskCompressed,
    Parked,
}

impl CounterBlock {
    pub(super) fn page_out(&self, placement: Placement) {
        let counter = match placement {
            Placement::Zero => &self.pages_out_zero,
            Placement::Store => &self.pages_out_store,
            Placement::DiskRaw => &self.pages_out_disk_raw,
            Placement::DiskCompressed => &self.pages_out_disk_compressed,
            Placement::Parked => &self.pages_out_parked,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A page the ladder had no destination for went back into memory.
    pub(super) fn put_back(&self) {
        self.put_back.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn rejected_incompressible(&self) {
        self.rejected_incompressible.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn rejected_full(&self) {
        self.rejected_full.fetch_add(1, Ordering::Relaxed);
    }

    /// A fault found its slot held by another path and waited.
    pub(super) fn contended_wait(&self) {
        self.contended_waits.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn disk_pages(&self, delta: i64) {
        self.disk_pages.fetch_add(delta, Ordering::Relaxed);
    }

    /// A fault from `source` was resolved after `wait_nanos`.
    pub(super) fn fault(&self, source: FaultSource, wait_nanos: u64) {
        let counter = &self.faults[source as usize];
        counter.count.fetch_add(1, Ordering::Relaxed);
        counter.wait_nanos.fetch_add(wait_nanos, Ordering::Relaxed);
    }
}

/// Every processor's block.
pub(super) struct Counters {
    blocks: Box<[CachePadded<CounterBlock>]>,
}

/// The counters summed over every processor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Totals {
    pub(super) pages_out_zero: u64,
    pub(super) pages_out_store: u64,
    pub(super) pages_out_disk_raw: u64,
    pub(super) pages_out_disk_compressed: u64,
    pub(super) pages_out_parked: u64,
    pub(super) put_back: u64,
    pub(super) rejected_incompressible: u64,
    pub(super) rejected_full: u64,
    pub(super) contended_waits: u64,
    pub(super) disk_pages: u64,
    pub(super) faults_zero: u64,
    pub(super) faults_store: u64,
    pub(super) faults_parked: u64,
    pub(super) faults_disk: u64,
    pub(super) fault_wait_nanos: u64,
}

impl Totals {
    /// Pages detached into a tier.
    pub(super) fn pages_out(&self) -> u64 {
        self.pages_out_zero
            + self.pages_out_store
            + self.pages_out_disk_raw
            + self.pages_out_disk_compressed
            + self.pages_out_parked
    }

    /// Faults resolved from any tier.
    pub(super) fn faults(&self) -> u64 {
        self.faults_zero + self.faults_store + self.faults_parked + self.faults_disk
    }
}

impl Counters {
    /// One block for each of `processors`.
    pub(super) fn new(processors: usize) -> Self {
        Self {
            blocks: (0..processors).map(|_| CachePadded::default()).collect(),
        }
    }

    /// The executing processor's block.
    pub(super) fn here(&self) -> &CounterBlock {
        let processor = usize::from(current_processor().id());
        self.blocks.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no swap counter block: the table was sized for {}",
                self.blocks.len()
            )
        })
    }

    pub(super) fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        let mut disk_pages = 0_i64;
        for block in &self.blocks {
            let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
            totals.pages_out_zero += load(&block.pages_out_zero);
            totals.pages_out_store += load(&block.pages_out_store);
            totals.pages_out_disk_raw += load(&block.pages_out_disk_raw);
            totals.pages_out_disk_compressed += load(&block.pages_out_disk_compressed);
            totals.pages_out_parked += load(&block.pages_out_parked);
            totals.put_back += load(&block.put_back);
            totals.rejected_incompressible += load(&block.rejected_incompressible);
            totals.rejected_full += load(&block.rejected_full);
            totals.contended_waits += load(&block.contended_waits);
            disk_pages += block.disk_pages.load(Ordering::Relaxed);
            for (source, field) in [
                (FaultSource::Zero, &mut totals.faults_zero),
                (FaultSource::Store, &mut totals.faults_store),
                (FaultSource::Parked, &mut totals.faults_parked),
                (FaultSource::Disk, &mut totals.faults_disk),
            ] {
                let counter = &block.faults[source as usize];
                *field += load(&counter.count);
                totals.fault_wait_nanos += load(&counter.wait_nanos);
            }
        }
        totals.disk_pages = u64::try_from(disk_pages).unwrap_or(0);
        totals
    }
}
