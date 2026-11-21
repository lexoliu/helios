#![cfg(feature = "runtime")]
//! The memory-map system calls against an address space of pages, checked
//! against the results Linux gives for the same calls (mm/mmap.c,
//! mm/mprotect.c, mm/mremap.c, mm/madvise.c).

use core::future::Future;
use core::ops::ControlFlow;
use core::task::{Context, Poll, Waker};
use std::collections::BTreeMap;

use helios_hal::vmm::PageFlags;
use helios_linux::abi::Abi;
use helios_linux::syscall::{
    Call, Clock, Denied, MapError, MemoryFault, MemoryTotals, NoBacking, Process, Region,
    SpawnError, SpawnedThread, Stdio, Stream, System, Thread, UserMemory, VirtualMemory,
    WriteError, dispatch,
};

const PAGE: u64 = 4096;
const LOWEST: u64 = PAGE;
const BREAK: u64 = 0x50_0000;
// x86-64 numbers (arch/x86/entry/syscalls/syscall_64.tbl).
const MMAP: u64 = 9;
const MPROTECT: u64 = 10;
const MUNMAP: u64 = 11;
const BRK: u64 = 12;
const MREMAP: u64 = 25;
const MADVISE: u64 = 28;
// Linux errno values.
const ENOMEM: u64 = 12;
const EFAULT: u64 = 14;
const EEXIST: u64 = 17;
const EINVAL: u64 = 22;
const RW: PageFlags = PageFlags::READ.union(PageFlags::WRITE);
const R: PageFlags = PageFlags::READ;
const RWX: PageFlags = RW.union(PageFlags::EXECUTE);

/// One mapped page: its access and its bytes, `None` once discarded.
struct Page {
    access: PageFlags,
    bytes: Option<Box<[u8; PAGE as usize]>>,
}

/// An address space of pages with the semantics the native space has.
#[derive(Default)]
struct Pages(BTreeMap<u64, Page>);

impl Pages {
    fn access(&self, base: u64) -> Option<PageFlags> {
        self.0.get(&base).map(|page| page.access)
    }
    fn byte(&self, address: u64) -> Option<u8> {
        let page = self.0.get(&(address & !(PAGE - 1)))?;
        Some(
            page.bytes
                .as_ref()
                .map_or(0, |bytes| bytes[(address % PAGE) as usize]),
        )
    }
    fn fill(&mut self, address: u64, length: u64, value: u8) {
        for address in address..address + length {
            let page = self.0.get_mut(&(address & !(PAGE - 1))).unwrap();
            page.bytes
                .get_or_insert_with(|| Box::new([0; PAGE as usize]))[(address % PAGE) as usize] =
                value;
        }
    }
    fn pages(first: u64, length: u64) -> impl Iterator<Item = u64> {
        (first..first + length).step_by(PAGE as usize)
    }
}

impl UserMemory for Pages {
    fn read(&self, address: u64, output: &mut [u8]) -> Result<(), MemoryFault> {
        for (offset, byte) in output.iter_mut().enumerate() {
            *byte = self.byte(address + offset as u64).ok_or(MemoryFault)?;
        }
        Ok(())
    }
    fn check_writable(&self, address: u64, length: usize) -> Result<(), MemoryFault> {
        (address..address + length as u64).try_for_each(|address| {
            match self.access(address & !(PAGE - 1)) {
                Some(access) if access.contains(PageFlags::WRITE) => Ok(()),
                _ => Err(MemoryFault),
            }
        })
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryFault> {
        self.check_writable(address, bytes.len())?;
        for (offset, byte) in bytes.iter().enumerate() {
            self.fill(address + offset as u64, 1, *byte);
        }
        Ok(())
    }
    fn atomic_load_u32(&self, address: u64) -> Result<u32, MemoryFault> {
        if address & 3 != 0 {
            return Err(MemoryFault);
        }
        let mut bytes = [0; 4];
        self.read(address, &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }
    fn atomic_store_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryFault> {
        if address & 3 != 0 {
            return Err(MemoryFault);
        }
        self.write(address, &value.to_le_bytes())
    }
    fn atomic_compare_exchange_u32(
        &mut self,
        address: u64,
        expected: u32,
        desired: u32,
    ) -> Result<Result<u32, u32>, MemoryFault> {
        let current = self.atomic_load_u32(address)?;
        if current == expected {
            self.atomic_store_u32(address, desired)?;
            Ok(Ok(current))
        } else {
            Ok(Err(current))
        }
    }
}

impl VirtualMemory for Pages {
    fn lowest_address(&self) -> u64 {
        LOWEST
    }
    fn map_anywhere(&mut self, hint: u64, length: u64, access: PageFlags) -> Result<u64, MapError> {
        let start = self.free_range(hint, length).ok_or(MapError::OutOfMemory)?;
        self.map_at(start, length, access)?;
        Ok(start)
    }
    fn map_at(&mut self, address: u64, length: u64, access: PageFlags) -> Result<(), MapError> {
        if address < LOWEST {
            return Err(MapError::Address);
        }
        if self.region(address, address + length).is_some() {
            return Err(MapError::Occupied);
        }
        for base in Self::pages(address, length) {
            self.0.insert(
                base,
                Page {
                    access,
                    bytes: None,
                },
            );
        }
        Ok(())
    }
    fn unmap(&mut self, address: u64, length: u64) {
        for base in Self::pages(address, length) {
            self.0.remove(&base);
        }
    }
    fn protect(&mut self, address: u64, length: u64, access: PageFlags) -> Result<(), MapError> {
        for base in Self::pages(address, length) {
            self.0
                .get_mut(&base)
                .expect("protect covers mapped pages")
                .access = access;
        }
        Ok(())
    }
    fn discard(&mut self, address: u64, length: u64) {
        for base in Self::pages(address, length) {
            if let Some(page) = self.0.get_mut(&base) {
                page.bytes = None;
            }
        }
    }
    fn remap(&mut self, from: u64, length: u64, to: u64, new_length: u64) -> Result<(), MapError> {
        let access = self.access(from).expect("remap moves a region");
        for base in Self::pages(from, length) {
            let page = self.0.remove(&base).expect("remap moves a region");
            self.0.insert(to + (base - from), page);
        }
        for base in Self::pages(to + length, new_length - length) {
            self.0.insert(
                base,
                Page {
                    access,
                    bytes: None,
                },
            );
        }
        Ok(())
    }
    fn region(&self, start: u64, end: u64) -> Option<Region> {
        let mut pages = self.0.range(start..end);
        let (&first, head) = pages.next()?;
        let mut last = first + PAGE;
        for (&base, page) in pages {
            if base != last || page.access != head.access {
                break;
            }
            last += PAGE;
        }
        Some(Region {
            start: first,
            end: last,
            access: head.access,
        })
    }
    fn free_range(&self, hint: u64, length: u64) -> Option<u64> {
        let mut candidate = hint.max(LOWEST);
        while self.region(candidate, candidate + length).is_some() {
            candidate = self.region(candidate, candidate + length)?.end;
        }
        Some(candidate)
    }
}

struct NoThread;
impl Thread for NoThread {
    fn thread_pointer(&self) -> u64 {
        0
    }
    fn set_thread_pointer(&mut self, _: u64) -> Result<(), Denied> {
        Err(Denied)
    }
    fn spawn_thread(&mut self, _: Process<NoBacking>, _: SpawnedThread) -> Result<(), SpawnError> {
        unreachable!("the memory tests spawn no threads")
    }
}

struct NoOutput;
impl Stdio for NoOutput {
    async fn write(&mut self, _: Stream, _: &[u8]) -> Result<usize, WriteError> {
        Err(WriteError::Io)
    }
    async fn read_input(&mut self, _: usize) -> Result<bytes::Bytes, Denied> {
        Err(Denied)
    }
}
impl Clock for NoOutput {
    fn now_nanos(&self) -> u64 {
        0
    }
    async fn sleep_until(&self, _: u64) {}
}
/// The memory calls read nothing of the machine; a test that reaches here
/// made a call it does not mean to.
impl System for NoOutput {
    fn realtime_nanos(&self) -> u64 {
        unreachable!("the memory tests read no clock")
    }
    fn set_realtime_nanos(&mut self, _: u64) -> Result<(), Denied> {
        Err(Denied)
    }
    fn cpu_nanos(&self) -> u64 {
        unreachable!("the memory tests read no CPU time")
    }
    fn fill_random(&mut self, _: &mut [u8]) {
        unreachable!("the memory tests draw no entropy")
    }
    fn memory(&self) -> MemoryTotals {
        unreachable!("the memory tests read no machine memory")
    }
    fn process_count(&self) -> u32 {
        unreachable!("the memory tests count no processes")
    }
    fn kernel_version(&self) -> &str {
        unreachable!("the memory tests read no kernel version")
    }
}

struct Linux {
    process: Process<NoBacking>,
    memory: Pages,
}

impl Linux {
    fn new() -> Self {
        Self {
            process: Process::new(Abi::X86_64, core::num::NonZeroU32::new(1).unwrap(), BREAK),
            memory: Pages::default(),
        }
    }

    fn call(&mut self, number: u64, arguments: [u64; 6]) -> u64 {
        let (mut thread, mut output) = (NoThread, NoOutput);
        let future = dispatch(
            &mut self.process,
            Call { number, arguments },
            &mut self.memory,
            &mut thread,
            &mut output,
        );
        let mut future = core::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(ControlFlow::Continue(result)) => result,
            other => panic!("memory call did not complete with a result: {other:?}"),
        }
    }

    /// A fresh anonymous read-write mapping of `pages` pages.
    fn map(&mut self, address: u64, pages: u64) -> u64 {
        // MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE.
        self.call(MMAP, [address, pages * PAGE, 3, 0x10_0022, u64::MAX, 0])
    }
}

fn error(errno: u64) -> u64 {
    errno.wrapping_neg()
}

#[test]
fn munmap_refuses_unaligned_empty_and_out_of_range_ranges_and_ignores_holes() {
    let mut linux = Linux::new();
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 4), base);
    // __vm_munmap: an unaligned start, a zero length, and a range past
    // TASK_SIZE are EINVAL; a range with nothing mapped succeeds.
    assert_eq!(
        linux.call(MUNMAP, [base + 1, PAGE, 0, 0, 0, 0]),
        error(EINVAL)
    );
    assert_eq!(linux.call(MUNMAP, [base, 0, 0, 0, 0, 0]), error(EINVAL));
    let task = Abi::X86_64.task_size();
    assert_eq!(
        linux.call(MUNMAP, [task - PAGE, 2 * PAGE, 0, 0, 0, 0]),
        error(EINVAL)
    );
    assert_eq!(linux.call(MUNMAP, [0, 2 * PAGE, 0, 0, 0, 0]), 0);
    // An unaligned length covers its last partial page; the middle of a
    // mapping splits it.
    assert_eq!(linux.call(MUNMAP, [base + PAGE, PAGE + 1, 0, 0, 0, 0]), 0);
    assert_eq!(linux.memory.access(base), Some(RW));
    assert_eq!(linux.memory.access(base + PAGE), None);
    assert_eq!(linux.memory.access(base + 2 * PAGE), None);
    assert_eq!(linux.memory.access(base + 3 * PAGE), Some(RW));
}

#[test]
fn mprotect_changes_regions_in_order_and_stops_at_the_first_hole() {
    let mut linux = Linux::new();
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 2), base);
    assert_eq!(linux.map(base + 3 * PAGE, 1), base + 3 * PAGE);
    // do_mprotect_pkey: alignment and both grow bits are EINVAL, a zero
    // length succeeds untouched, an unmapped start is ENOMEM.
    assert_eq!(
        linux.call(MPROTECT, [base + 1, PAGE, 1, 0, 0, 0]),
        error(EINVAL)
    );
    assert_eq!(
        linux.call(MPROTECT, [base, PAGE, 0x0300_0001, 0, 0, 0]),
        error(EINVAL)
    );
    assert_eq!(linux.call(MPROTECT, [base, 0, 1, 0, 0, 0]), 0);
    assert_eq!(
        linux.call(MPROTECT, [base - PAGE, 2 * PAGE, 1, 0, 0, 0]),
        error(ENOMEM)
    );
    assert_eq!(linux.memory.access(base), Some(RW));
    // arm64's PROT_BTI is no generic protection bit.
    assert_eq!(
        linux.call(MPROTECT, [base, PAGE, 0x11, 0, 0, 0]),
        error(EINVAL)
    );
    // The hole at base + 2 pages ends the walk after the first region
    // changed; the region past it keeps its access.
    assert_eq!(
        linux.call(MPROTECT, [base, 4 * PAGE, 1, 0, 0, 0]),
        error(ENOMEM)
    );
    assert_eq!(linux.memory.access(base), Some(R));
    assert_eq!(linux.memory.access(base + PAGE), Some(R));
    assert_eq!(linux.memory.access(base + 3 * PAGE), Some(RW));
    // PROT_NONE keeps the pages mapped.
    assert_eq!(linux.call(MPROTECT, [base, 2 * PAGE, 0, 0, 0, 0]), 0);
    assert_eq!(linux.memory.access(base), Some(PageFlags::empty()));
}

#[test]
fn write_and_execute_together_are_granted_as_on_linux() {
    // mmap(2) and mprotect(2) grant PROT_WRITE | PROT_EXEC on anonymous
    // private memory: `calc_vm_prot_bits` sets VM_WRITE | VM_EXEC and
    // anonymous mappings carry every VM_MAY* bit.
    let mut linux = Linux::new();
    let rwx = linux.call(MMAP, [0, PAGE, 7, 0x22, u64::MAX, 0]);
    assert_eq!(linux.memory.access(rwx), Some(RWX));
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 1), base);
    assert_eq!(linux.call(MPROTECT, [base, PAGE, 7, 0, 0, 0]), 0);
    assert_eq!(linux.memory.access(base), Some(RWX));
    // Write and execute without an explicit read: both ISAs read it too.
    assert_eq!(linux.call(MPROTECT, [base, PAGE, 6, 0, 0, 0]), 0);
    assert_eq!(linux.memory.access(base), Some(RWX));
}

#[test]
fn madvise_drops_every_mapped_part_and_reports_holes() {
    let mut linux = Linux::new();
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 1), base);
    assert_eq!(linux.map(base + 2 * PAGE, 1), base + 2 * PAGE);
    linux.memory.fill(base, PAGE, 0x5a);
    linux.memory.fill(base + 2 * PAGE, PAGE, 0x5a);
    // madvise_walk_vmas: both mapped pages are dropped, and the hole
    // between them makes the call ENOMEM.
    assert_eq!(
        linux.call(MADVISE, [base, 3 * PAGE, 4, 0, 0, 0]),
        error(ENOMEM)
    );
    assert_eq!(linux.memory.byte(base), Some(0));
    assert_eq!(linux.memory.byte(base + 2 * PAGE), Some(0));
    linux.memory.fill(base, PAGE, 0x5a);
    assert_eq!(linux.call(MADVISE, [base, PAGE, 8, 0, 0, 0]), 0);
    assert_eq!(linux.memory.byte(base + 17), Some(0));
    assert_eq!(linux.call(MADVISE, [base, 0, 4, 0, 0, 0]), 0);
    assert_eq!(
        linux.call(MADVISE, [base + 1, PAGE, 4, 0, 0, 0]),
        error(EINVAL)
    );
    assert_eq!(
        linux.call(MADVISE, [base, u64::MAX, 4, 0, 0, 0]),
        error(EINVAL)
    );
    // MADV_HUGEPAGE (14) is advice this kernel does not take.
    assert_eq!(
        linux.call(MADVISE, [base, PAGE, 14, 0, 0, 0]),
        error(EINVAL)
    );
}

#[test]
fn mremap_shrinks_grows_in_place_and_moves_contents() {
    let mut linux = Linux::new();
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 2), base);
    linux.memory.fill(base, 2 * PAGE, 0x33);
    // Grow in place into the free pages after the region.
    assert_eq!(
        linux.call(MREMAP, [base, 2 * PAGE, 4 * PAGE, 0, 0, 0]),
        base
    );
    assert_eq!(linux.memory.access(base + 3 * PAGE), Some(RW));
    assert_eq!(linux.memory.byte(base + PAGE), Some(0x33));
    assert_eq!(linux.memory.byte(base + 3 * PAGE), Some(0));
    // A mapping right after blocks growth: ENOMEM without MREMAP_MAYMOVE,
    // a move with it.
    assert_eq!(linux.map(base + 4 * PAGE, 1), base + 4 * PAGE);
    assert_eq!(
        linux.call(MREMAP, [base, 4 * PAGE, 8 * PAGE, 0, 0, 0]),
        error(ENOMEM)
    );
    let moved = linux.call(MREMAP, [base, 4 * PAGE, 8 * PAGE, 1, 0, 0]);
    assert_ne!(moved, base);
    assert_eq!(linux.memory.access(base), None);
    assert_eq!(linux.memory.byte(moved + PAGE), Some(0x33));
    assert_eq!(linux.memory.access(moved + 7 * PAGE), Some(RW));
    // Shrinking unmaps the tail.
    assert_eq!(linux.call(MREMAP, [moved, 8 * PAGE, PAGE, 0, 0, 0]), moved);
    assert_eq!(linux.memory.access(moved + PAGE), None);
}

#[test]
fn mremap_refuses_what_linux_refuses() {
    let mut linux = Linux::new();
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 2), base);
    assert_eq!(
        linux.call(MPROTECT, [base + PAGE, PAGE, 1, 0, 0, 0]),
        0,
        "two regions"
    );
    // Flags: unknown, FIXED without MAYMOVE, DONTUNMAP with a resize.
    for flags in [8, 2, 5] {
        assert_eq!(
            linux.call(MREMAP, [base, PAGE, 2 * PAGE, flags, 0, 0]),
            error(EINVAL)
        );
    }
    assert_eq!(linux.call(MREMAP, [base, PAGE, 0, 0, 0, 0]), error(EINVAL));
    // A shrink whose tail runs past TASK_SIZE: do_vmi_munmap refuses it.
    assert_eq!(
        linux.call(MREMAP, [base, u64::MAX - PAGE, PAGE, 0, 0, 0]),
        error(EINVAL)
    );
    // vma_lookup: nothing mapped at the address; vma_to_resize: a range
    // crossing into the next region, and a zero old length of private
    // memory.
    assert_eq!(
        linux.call(MREMAP, [base + 8 * PAGE, PAGE, 2 * PAGE, 1, 0, 0]),
        error(EFAULT)
    );
    assert_eq!(
        linux.call(MREMAP, [base, 2 * PAGE, 3 * PAGE, 1, 0, 0]),
        error(EFAULT)
    );
    assert_eq!(linux.call(MREMAP, [base, 0, PAGE, 1, 0, 0]), error(EINVAL));
    // mremap_to: an overlapping or unaligned target.
    assert_eq!(
        linux.call(MREMAP, [base, PAGE, PAGE, 3, base, 0]),
        error(EINVAL)
    );
    assert_eq!(
        linux.call(MREMAP, [base, PAGE, PAGE, 3, 0x20_0001, 0]),
        error(EINVAL)
    );
}

#[test]
fn mremap_fixed_replaces_the_target_and_dontunmap_leaves_zeros() {
    let mut linux = Linux::new();
    let (base, target) = (0x10_0000, 0x20_0000);
    assert_eq!(linux.map(base, 1), base);
    assert_eq!(linux.map(target, 2), target);
    linux.memory.fill(base, PAGE, 0x44);
    linux.memory.fill(target, 2 * PAGE, 0x11);
    // MREMAP_MAYMOVE | MREMAP_FIXED unmaps the new length at the target
    // first, and nothing past it.
    assert_eq!(linux.call(MREMAP, [base, PAGE, PAGE, 3, target, 0]), target);
    assert_eq!(linux.memory.byte(target), Some(0x44));
    assert_eq!(linux.memory.byte(target + PAGE), Some(0x11));
    assert_eq!(linux.memory.access(base), None);
    // MREMAP_MAYMOVE | MREMAP_DONTUNMAP keeps the source mapped and empty.
    let moved = linux.call(MREMAP, [target, PAGE, PAGE, 5, 0x30_0000, 0]);
    assert_eq!(moved, 0x30_0000);
    assert_eq!(linux.memory.byte(moved), Some(0x44));
    assert_eq!(linux.memory.access(target), Some(RW));
    assert_eq!(linux.memory.byte(target), Some(0));
}

#[test]
fn brk_moves_within_the_free_pages_above_its_start() {
    let mut linux = Linux::new();
    // brk(0) and any request below the start report the current break.
    assert_eq!(linux.call(BRK, [0; 6]), BREAK);
    assert_eq!(linux.call(BRK, [BREAK - 1, 0, 0, 0, 0, 0]), BREAK);
    // Growth maps the pages up to the new break, whose unaligned value is
    // kept; a request in the same page only records it.
    assert_eq!(
        linux.call(BRK, [BREAK + 2 * PAGE + 5, 0, 0, 0, 0, 0]),
        BREAK + 2 * PAGE + 5
    );
    assert_eq!(linux.memory.access(BREAK + 2 * PAGE), Some(RW));
    assert_eq!(
        linux.call(BRK, [BREAK + 3 * PAGE, 0, 0, 0, 0, 0]),
        BREAK + 3 * PAGE
    );
    // Growth must leave one free page below the next mapping.
    let blocker = BREAK + 8 * PAGE;
    assert_eq!(linux.map(blocker, 1), blocker);
    assert_eq!(
        linux.call(BRK, [blocker - PAGE + 1, 0, 0, 0, 0, 0]),
        BREAK + 3 * PAGE
    );
    assert_eq!(
        linux.call(BRK, [blocker - PAGE, 0, 0, 0, 0, 0]),
        blocker - PAGE
    );
    assert_eq!(linux.memory.access(blocker - 2 * PAGE), Some(RW));
    assert_eq!(linux.memory.access(blocker - PAGE), None);
    // Shrinking unmaps; past TASK_SIZE is refused.
    assert_eq!(linux.call(BRK, [BREAK + PAGE, 0, 0, 0, 0, 0]), BREAK + PAGE);
    assert_eq!(linux.memory.access(BREAK + PAGE), None);
    assert_eq!(linux.memory.access(BREAK), Some(RW));
    assert_eq!(linux.call(BRK, [u64::MAX, 0, 0, 0, 0, 0]), BREAK + PAGE);
}

#[test]
fn map_fixed_replaces_and_noreplace_refuses_an_existing_mapping() {
    let mut linux = Linux::new();
    let base = 0x10_0000;
    assert_eq!(linux.map(base, 2), base);
    linux.memory.fill(base, 2 * PAGE, 0x77);
    // MAP_FIXED_NOREPLACE: EEXIST over a mapping.
    assert_eq!(linux.map(base + PAGE, 1), error(EEXIST));
    // MAP_FIXED, PROT_NONE: replaced with fresh, inaccessible pages.
    assert_eq!(
        linux.call(MMAP, [base + PAGE, PAGE, 0, 0x32, u64::MAX, 0]),
        base + PAGE
    );
    assert_eq!(linux.memory.access(base + PAGE), Some(PageFlags::empty()));
    assert_eq!(linux.memory.byte(base + PAGE), Some(0));
    assert_eq!(linux.memory.byte(base), Some(0x77));
    // An unaligned fixed address is EINVAL; one past TASK_SIZE is ENOMEM.
    assert_eq!(
        linux.call(MMAP, [base + 1, PAGE, 3, 0x32, u64::MAX, 0]),
        error(EINVAL)
    );
    let task = Abi::X86_64.task_size();
    assert_eq!(
        linux.call(MMAP, [task, PAGE, 3, 0x32, u64::MAX, 0]),
        error(ENOMEM)
    );
}
