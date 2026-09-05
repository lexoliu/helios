use core::ops::Range;

use arrayvec::ArrayVec;

/// How many bootloader-owned ranges a machine may hold back from the
/// usable memory map.
pub const MAX_BOOT_RESERVED_RANGES: usize = 4;

/// How many usable pieces one memory region can be cut into once every
/// reserved range is subtracted from it.
pub const MAX_USABLE_REGION_SEGMENTS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMemoryKind {
    Usable,
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootMemoryRegion {
    pub start: u64,
    pub end: u64,
    pub kind: BootMemoryKind,
}

impl BootMemoryRegion {
    pub const fn usable(self) -> bool {
        matches!(self.kind, BootMemoryKind::Usable)
    }
}

pub trait BootMemoryMap {
    type Iter: Iterator<Item = BootMemoryRegion>;

    fn regions(&self) -> Self::Iter;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareKind {
    Uefi32,
    Uefi64,
    Sbi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootKernelImage {
    pub physical_base: u64,
    pub virtual_base: u64,
    pub file_address: usize,
    pub size: u64,
}

/// Where the bootloader left the firmware's own descriptions of the
/// machine, for whichever of them this firmware publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootFirmwareTables {
    /// Physical address of the ACPI root pointer.
    ///
    /// Physical rather than mapped, because an ACPI table walk follows
    /// physical addresses out of the tables themselves and has to map
    /// each one the same way; handing it a pre-mapped root would make
    /// the root the one address that is translated differently.
    pub acpi_rsdp: Option<usize>,
    /// Address of the flattened device tree, mapped and readable.
    ///
    /// Unlike ACPI, a device tree is one self-contained blob: nothing
    /// in it is reached by physical address, so the only thing a reader
    /// needs is somewhere to read it from.
    pub device_tree_blob: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootModule<'a> {
    pub address: usize,
    pub size: usize,
    pub path: &'a [u8],
    pub command_line: &'a [u8],
}

pub trait BootModules<'a> {
    type Iter: Iterator<Item = BootModule<'a>>;

    fn modules(&self) -> Self::Iter;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoBootModules;

impl<'a> BootModules<'a> for NoBootModules {
    type Iter = core::iter::Empty<BootModule<'a>>;

    fn modules(&self) -> Self::Iter {
        core::iter::empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootHandoff<'a, MemoryMap, Modules> {
    pub memory_map: MemoryMap,
    pub kernel: BootKernelImage,
    pub command_line: &'a [u8],
    pub modules: Modules,
    pub firmware: FirmwareKind,
    pub tables: BootFirmwareTables,
}

/// The bootloader-owned ranges carved out of the usable memory map.
///
/// A bootloader hands the kernel a memory map that still describes the
/// bytes it put the kernel image in as usable, and a machine may need
/// one more range held back for its own bring-up. Each of those is
/// recorded here and subtracted from every usable region before the
/// kernel primes an allocator from it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootReservedRanges {
    ranges: ArrayVec<Range<usize>, MAX_BOOT_RESERVED_RANGES>,
}

impl BootReservedRanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds `range` back from every usable region.
    ///
    /// Panics past capacity rather than dropping the range: a reserved
    /// range that silently went missing hands the allocator memory the
    /// firmware or the kernel image still owns.
    pub fn reserve(&mut self, range: Range<usize>) {
        self.ranges.try_push(range).unwrap_or_else(|error| {
            panic!(
                "boot reserved ranges exceeded capacity {MAX_BOOT_RESERVED_RANGES}: {:#x?}",
                error.element()
            )
        });
    }

    pub fn iter(&self) -> impl Iterator<Item = &Range<usize>> {
        self.ranges.iter()
    }
}

/// The usable pieces of `region` once every reserved range is subtracted.
///
/// A region that is not usable, or empty, yields nothing. Each reserved
/// range that overlaps a piece splits it in two, so the count is bounded
/// by the reserved-range capacity and returned in a fixed-size array
/// rather than an allocation.
pub fn usable_region_segments(
    region: BootMemoryRegion,
    reserved_ranges: &BootReservedRanges,
) -> [Option<Range<usize>>; MAX_USABLE_REGION_SEGMENTS] {
    if !region.usable() || region.end <= region.start {
        return [const { None }; MAX_USABLE_REGION_SEGMENTS];
    }

    let mut segments = [const { None }; MAX_USABLE_REGION_SEGMENTS];
    segments[0] = Some(region.start as usize..region.end as usize);
    for reserved in reserved_ranges.iter() {
        segments = subtract_reserved_range(segments, reserved);
    }
    segments
}

fn subtract_reserved_range(
    segments: [Option<Range<usize>>; MAX_USABLE_REGION_SEGMENTS],
    reserved: &Range<usize>,
) -> [Option<Range<usize>>; MAX_USABLE_REGION_SEGMENTS] {
    let mut result = [const { None }; MAX_USABLE_REGION_SEGMENTS];
    let mut next = 0;
    for segment in segments.into_iter().flatten() {
        for piece in split_segment(segment, reserved).into_iter().flatten() {
            assert!(
                next < MAX_USABLE_REGION_SEGMENTS,
                "boot memory segmentation exceeded capacity {MAX_USABLE_REGION_SEGMENTS}"
            );
            result[next] = Some(piece);
            next += 1;
        }
    }
    result
}

/// `segment` with `reserved` taken out of it: the part below it and the
/// part above it, either of which may be empty.
fn split_segment(segment: Range<usize>, reserved: &Range<usize>) -> [Option<Range<usize>>; 2] {
    if reserved.end <= segment.start || reserved.start >= segment.end {
        return [Some(segment), None];
    }
    let below = (reserved.start > segment.start).then_some(segment.start..reserved.start);
    let above = (reserved.end < segment.end).then_some(reserved.end..segment.end);
    [below, above]
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{BootMemoryKind, BootMemoryRegion, BootReservedRanges, usable_region_segments};
    use core::ops::Range;

    #[test]
    fn usable_region_without_overlap_stays_single_segment() {
        assert_eq!(
            collect_segments(region(0x1000..0x4000), Some(0x5000..0x6000)),
            alloc::vec![0x1000..0x4000]
        );
    }

    #[test]
    fn excluded_wakeup_page_splits_region_without_heap_growth_path() {
        assert_eq!(
            collect_segments(region(0x1000..0x5000), Some(0x2000..0x3000)),
            alloc::vec![0x1000..0x2000, 0x3000..0x5000]
        );
    }

    #[test]
    fn non_usable_region_yields_no_segments() {
        assert!(
            collect_segments(
                BootMemoryRegion {
                    start: 0x1000,
                    end: 0x4000,
                    kind: BootMemoryKind::Reserved,
                },
                None,
            )
            .is_empty()
        );
    }

    #[test]
    fn every_reserved_range_is_subtracted() {
        let mut reserved = BootReservedRanges::new();
        reserved.reserve(0x2000..0x3000);
        reserved.reserve(0x6000..0x7000);

        assert_eq!(
            usable_region_segments(region(0x1000..0x9000), &reserved)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            alloc::vec![0x1000..0x2000, 0x3000..0x6000, 0x7000..0x9000]
        );
    }

    fn collect_segments(
        region: BootMemoryRegion,
        excluded: Option<Range<usize>>,
    ) -> Vec<Range<usize>> {
        let mut reserved = BootReservedRanges::new();
        if let Some(excluded) = excluded {
            reserved.reserve(excluded);
        }
        usable_region_segments(region, &reserved)
            .into_iter()
            .flatten()
            .collect()
    }

    fn region(range: Range<usize>) -> BootMemoryRegion {
        BootMemoryRegion {
            start: range.start as u64,
            end: range.end as u64,
            kind: BootMemoryKind::Usable,
        }
    }
}
