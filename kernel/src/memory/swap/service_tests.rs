//! The swap service's failure paths, driven against a backend that fails
//! on demand and an address space that models what the real ones do to a
//! page-table entry.
//!
//! The service is private to the kernel and needs no executor to be
//! driven: a fault is issued by polling `fault_in` once (which queues the
//! request and parks), the service is run by hand, and the fault is
//! polled again for its result. Tests that need two requests in the queue
//! at once issue both before running the service.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::Mutex;

use helios_hal::vmm::{AddressSpaceError, PageFlags};

use super::*;
use crate::test_support::TestCpu;

const PAGE: usize = PhysFrame::SIZE;

/// What the fake address space holds for one page.
enum FakePage {
    Present(Vec<u8>),
    Swapped(SwapToken),
}

#[derive(Default)]
struct FakeSpace {
    pages: BTreeMap<usize, FakePage>,
    /// How many of the next `swap_in_page` calls refuse, leaving the entry
    /// as it was, the way the real address spaces do.
    reinstatements_to_refuse: u32,
    orphans: Vec<SwapToken>,
}

std::thread_local! {
    static SPACE: RefCell<FakeSpace> = RefCell::new(FakeSpace::default());
    static CLOCK_NANOS: Cell<u64> = const { Cell::new(0) };
}

fn with_space<R>(f: impl FnOnce(&mut FakeSpace) -> R) -> R {
    SPACE.with(|space| f(&mut space.borrow_mut()))
}

fn set_clock(nanos: u64) {
    CLOCK_NANOS.with(|clock| clock.set(nanos));
}

static TEST_HOOKS: SwapVmHooks = SwapVmHooks {
    swap_out_page: |addr, token, out| {
        with_space(|space| {
            let page = space
                .pages
                .get_mut(&addr.raw())
                .ok_or(AddressSpaceError::NotCommitted)?;
            let FakePage::Present(bytes) = page else {
                return Err(AddressSpaceError::NotCommitted);
            };
            out.copy_from_slice(bytes);
            *page = FakePage::Swapped(token);
            Ok(PageFlags::READ | PageFlags::WRITE)
        })
    },
    swap_in_page: |addr, bytes| {
        with_space(|space| {
            if space.reinstatements_to_refuse > 0 {
                space.reinstatements_to_refuse -= 1;
                return Err(AddressSpaceError::OutOfFrames);
            }
            let page = space
                .pages
                .get_mut(&addr.raw())
                .ok_or(AddressSpaceError::NotSwapped)?;
            let FakePage::Swapped(token) = *page else {
                return Err(AddressSpaceError::NotSwapped);
            };
            *page = FakePage::Present(bytes.to_vec());
            Ok(token)
        })
    },
    swapped_token: |addr| {
        with_space(|space| match space.pages.get(&addr.page_floor().raw()) {
            Some(FakePage::Swapped(token)) => Some(*token),
            _ => None,
        })
    },
    scan_committed_pages: |_, _, _| 0,
    owned_resident_bytes: |_| 0,
    drain_orphaned_swap_tokens: |context, visit| {
        let orphans = with_space(|space| core::mem::take(&mut space.orphans));
        let count = orphans.len();
        for token in orphans {
            visit(context, token);
        }
        count
    },
    now_ticks: || CLOCK_NANOS.with(Cell::get),
};

#[derive(Debug, Error)]
#[error("injected swap backend failure")]
struct Injected;

#[derive(Default)]
struct FlakyState {
    next_token: u64,
    extents: BTreeMap<u64, Vec<u8>>,
    reads_to_fail: u32,
    writes_to_refuse: u32,
    /// Every token passed to `release`, in call order.
    released: Vec<u64>,
}

/// A backend whose reads and writes fail on demand. A failed read leaves
/// the extent in place and a successful one consumes it, the contract the
/// virtio backend keeps.
#[derive(Clone, Default)]
struct FlakyBackend {
    state: Arc<Mutex<FlakyState>>,
}

impl FlakyBackend {
    fn fail_reads(&self, count: u32) {
        self.state.lock().unwrap().reads_to_fail = count;
    }

    fn refuse_writes(&self, count: u32) {
        self.state.lock().unwrap().writes_to_refuse = count;
    }

    fn released(&self) -> Vec<u64> {
        self.state.lock().unwrap().released.clone()
    }

    fn extents(&self) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .extents
            .values()
            .cloned()
            .collect()
    }
}

impl SwapBackend for FlakyBackend {
    type Token = u64;
    type Error = Injected;

    async fn swap_out(&self, bytes: &[u8]) -> Result<Self::Token, Self::Error> {
        let mut state = self.state.lock().unwrap();
        if state.writes_to_refuse > 0 {
            state.writes_to_refuse -= 1;
            return Err(Injected);
        }
        state.next_token += 1;
        let token = state.next_token;
        state.extents.insert(token, bytes.to_vec());
        Ok(token)
    }

    async fn swap_in(&self, token: Self::Token, dst: &mut [u8]) -> Result<(), Self::Error> {
        let mut state = self.state.lock().unwrap();
        if state.reads_to_fail > 0 {
            state.reads_to_fail -= 1;
            return Err(Injected);
        }
        let bytes = state.extents.remove(&token).expect("a live token");
        dst.copy_from_slice(&bytes);
        Ok(())
    }

    async fn release(&self, token: Self::Token) {
        let mut state = self.state.lock().unwrap();
        state.released.push(token);
        state
            .extents
            .remove(&token)
            .expect("a release only ever names a token that swap_in has not consumed");
    }
}

struct Harness {
    service: SwapService<FlakyBackend, TestCpu>,
    handle: SwapHandle,
    backend: FlakyBackend,
}

fn block_on<F: Future>(future: F) -> F::Output {
    futures_lite::future::block_on(future)
}

impl Harness {
    fn new() -> Self {
        with_space(|space| *space = FakeSpace::default());
        set_clock(0);
        let backend = FlakyBackend::default();
        let handle = SwapHandle::new(
            "test",
            0,
            SwapClock {
                timebase: Timebase::new(1_000_000_000),
                now_ticks: TEST_HOOKS.now_ticks,
            },
        );
        let service = SwapService {
            backend: backend.clone(),
            handle: handle.clone(),
            hooks: &TEST_HOOKS,
            registry: InstanceRegistry::new(),
            cpu: TestCpu::with_entropy(0),
            map: SwapMap::new(),
            page: vec![0_u8; PAGE].into_boxed_slice(),
            report: SwapReport::default(),
        };
        Self {
            service,
            handle,
            backend,
        }
    }

    /// A resident page at page number `number`, filled with a pattern
    /// that differs per page and per byte.
    fn resident(&self, number: usize, seed: u8) -> VirtAddr {
        let addr = VirtAddr::new(number * PAGE);
        with_space(|space| {
            space
                .pages
                .insert(addr.raw(), FakePage::Present(pattern(seed)));
        });
        addr
    }

    fn evict(&mut self, addr: VirtAddr) -> Result<(), ()> {
        block_on(self.service.swap_out(addr))
    }

    /// The index the page-table entry carries, if the page is swapped.
    fn index_of(&self, addr: VirtAddr) -> Option<SwapToken> {
        (TEST_HOOKS.swapped_token)(addr)
    }

    fn contents(&self, addr: VirtAddr) -> Vec<u8> {
        with_space(|space| match &space.pages[&addr.raw()] {
            FakePage::Present(bytes) => bytes.clone(),
            FakePage::Swapped(_) => panic!("page {:#x} is not resident", addr.raw()),
        })
    }

    fn serve(&mut self) {
        block_on(self.service.serve_faults());
    }

    fn fault(&mut self, addr: VirtAddr) -> Result<(), SwapFaultError> {
        let handle = self.handle.clone();
        let mut fault = issue(&handle, addr);
        self.serve();
        poll(&mut fault).expect("the service settled the fault")
    }

    fn teardown(&mut self, index: SwapToken) {
        with_space(|space| space.orphans.push(index));
        block_on(self.service.release_orphaned_tokens());
    }
}

fn pattern(seed: u8) -> Vec<u8> {
    (0..PAGE)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

/// Queues a fault the way the trampoline does, without running the
/// service.
fn issue(
    handle: &SwapHandle,
    addr: VirtAddr,
) -> Pin<Box<impl Future<Output = Result<(), SwapFaultError>>>> {
    let mut fault = Box::pin(handle.fault_in(addr));
    assert!(
        poll(&mut fault).is_none(),
        "a fault parks until the service has run"
    );
    fault
}

fn poll<T>(future: &mut Pin<Box<impl Future<Output = T>>>) -> Option<T> {
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

#[test]
fn a_failed_read_leaves_the_entry_and_the_extent_as_they_were() {
    let mut harness = Harness::new();
    let first = harness.resident(1, 0x10);
    let second = harness.resident(2, 0x20);
    harness.evict(first).expect("evict first");
    let index = harness.index_of(first).expect("first is swapped");

    harness.backend.fail_reads(1);
    assert_eq!(harness.fault(first), Err(SwapFaultError::Backend));
    assert_eq!(
        harness.index_of(first),
        Some(index),
        "the page-table entry still names the index"
    );
    assert_eq!(harness.backend.extents(), [pattern(0x10)]);

    // Another page going out in between must not be handed the index the
    // first page's entry still carries.
    harness.evict(second).expect("evict second");
    assert_ne!(
        harness.index_of(second),
        Some(index),
        "an index a page-table entry still names is not free"
    );

    harness.fault(first).expect("the retry succeeds");
    assert_eq!(harness.contents(first), pattern(0x10));
    harness.fault(second).expect("the second page is unharmed");
    assert_eq!(harness.contents(second), pattern(0x20));
}

#[test]
fn a_failed_reinstatement_sends_the_page_back_under_the_same_index() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0x30);
    harness.evict(addr).expect("evict");
    let index = harness.index_of(addr).expect("swapped");

    with_space(|space| space.reinstatements_to_refuse = 1);
    assert_eq!(harness.fault(addr), Err(SwapFaultError::AddressSpace));
    assert_eq!(
        harness.index_of(addr),
        Some(index),
        "the entry still names the index"
    );
    assert_eq!(
        harness.backend.extents(),
        [pattern(0x30)],
        "the backend holds the page again, because the staging buffer held its only copy"
    );

    harness.fault(addr).expect("the next fault succeeds");
    assert_eq!(harness.contents(addr), pattern(0x30));
}

#[test]
fn a_page_that_cannot_go_back_to_the_backend_is_lost_and_its_index_stays_reserved() {
    let mut harness = Harness::new();
    let lost = harness.resident(1, 0x40);
    let other = harness.resident(2, 0x50);
    harness.evict(lost).expect("evict");
    let index = harness.index_of(lost).expect("swapped");

    with_space(|space| space.reinstatements_to_refuse = 1);
    harness.backend.refuse_writes(1);
    assert_eq!(harness.fault(lost), Err(SwapFaultError::AddressSpace));
    assert_eq!(harness.index_of(lost), Some(index));
    assert_eq!(harness.backend.extents(), Vec::<Vec<u8>>::new());

    assert_eq!(
        harness.fault(lost),
        Err(SwapFaultError::PageLost),
        "a fault on a lost page reports it, so the owning instance traps"
    );

    harness.evict(other).expect("evict other");
    assert_ne!(
        harness.index_of(other),
        Some(index),
        "a lost page's index stays reserved"
    );

    // Tearing the address space down releases the reservation.
    harness.teardown(index);
    let third = harness.resident(3, 0x60);
    harness.evict(third).expect("evict third");
    assert_eq!(harness.index_of(third), Some(index));
}

#[test]
fn a_refused_write_puts_the_page_back() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0x70);
    harness.backend.refuse_writes(1);
    assert_eq!(harness.evict(addr), Err(()));
    assert_eq!(harness.contents(addr), pattern(0x70));
    assert_eq!(harness.index_of(addr), None);

    harness.evict(addr).expect("the next eviction succeeds");
    harness.fault(addr).expect("fault");
    assert_eq!(harness.contents(addr), pattern(0x70));
}

#[test]
fn a_refused_write_with_a_failed_put_back_loses_the_page_loudly() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0x80);
    // The backend refuses the write and the retry of it, and the address
    // space refuses the put-back.
    harness.backend.refuse_writes(2);
    with_space(|space| space.reinstatements_to_refuse = 1);
    assert_eq!(harness.evict(addr), Err(()));
    let index = harness
        .index_of(addr)
        .expect("the entry still names an index");

    assert_eq!(harness.fault(addr), Err(SwapFaultError::PageLost));
    let other = harness.resident(2, 0x90);
    harness.evict(other).expect("evict other");
    assert_ne!(harness.index_of(other), Some(index));
    harness.teardown(index);
}

#[test]
fn a_failed_put_back_that_the_backend_then_accepts_leaves_the_page_swapped_out() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0xa0);
    harness.backend.refuse_writes(1);
    with_space(|space| space.reinstatements_to_refuse = 1);
    harness
        .evict(addr)
        .expect("the retry of the write lands, so the page is out");
    assert!(harness.index_of(addr).is_some());
    harness.fault(addr).expect("fault");
    assert_eq!(harness.contents(addr), pattern(0xa0));
}

#[test]
fn teardown_releases_a_swapped_out_page_once_and_never_a_swapped_in_one() {
    let mut harness = Harness::new();
    let back = harness.resident(1, 0xc0);
    let out = harness.resident(2, 0xc1);
    harness.evict(back).expect("evict the page that returns");
    harness.evict(out).expect("evict the page that stays out");
    let back_index = harness.index_of(back).expect("swapped out");
    let out_index = harness.index_of(out).expect("swapped out");
    harness.fault(back).expect("swap the first page back in");
    assert_eq!(harness.backend.released(), Vec::<u64>::new());

    // The address space's teardown hands over the index of every page it
    // ever swapped out, the swapped-in one included.
    harness.teardown(back_index);
    assert_eq!(
        harness.backend.released(),
        Vec::<u64>::new(),
        "a token swap_in consumed is never released"
    );
    harness.teardown(out_index);
    harness.teardown(out_index);
    assert_eq!(
        harness.backend.released(),
        vec![2],
        "the page still out is released once, under its own token"
    );
}

#[test]
fn two_faults_on_one_page_both_succeed() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0xb0);
    harness.evict(addr).expect("evict");

    let handle = harness.handle.clone();
    let mut first = issue(&handle, addr);
    let mut second = issue(&handle, VirtAddr::new(addr.raw() + 17));
    harness.serve();
    assert_eq!(poll(&mut first), Some(Ok(())));
    assert_eq!(
        poll(&mut second),
        Some(Ok(())),
        "the page is present, so the instruction retries"
    );
    assert_eq!(harness.contents(addr), pattern(0xb0));
    assert_eq!(harness.handle.stats().pages_in, 1);
}

#[test]
fn fault_latency_counts_the_time_a_request_spent_queued() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0xc0);
    harness.evict(addr).expect("evict");

    set_clock(1_000);
    let handle = harness.handle.clone();
    let mut fault = issue(&handle, addr);
    // The service is busy elsewhere for fifty microseconds before it gets
    // to the request; the read itself takes no time on the test clock.
    set_clock(51_000);
    harness.serve();
    assert_eq!(poll(&mut fault), Some(Ok(())));

    let stats = harness.handle.stats();
    assert_eq!(stats.faults_served, 1);
    assert_eq!(
        stats.mean_fault_latency_nanos, 50_000,
        "latency runs from the request to its completion, queue included"
    );
}

/// The checks a host call or a runtime copy makes, over the fake address
/// space and the harness's swap service.
fn residency(handle: &SwapHandle) -> GuestResidency {
    GuestResidency {
        hooks: &TEST_HOOKS,
        // Leaked: a residency names its handle `'static`, as the machine's
        // one swap handle is.
        handle: Box::leak(Box::new(Once::initialized(handle.clone()))),
    }
}

/// Stands in for a host call's copy: writes `byte` at the start of each
/// page, and refuses to run while any of them is out.
fn copy_into(pages: &[VirtAddr], byte: u8) {
    with_space(|space| {
        for page in pages {
            match space.pages.get_mut(&page.raw()) {
                Some(FakePage::Present(bytes)) => bytes[0] = byte,
                _ => panic!("the copy ran while page {:#x} was out", page.raw()),
            }
        }
    });
}

/// The issue's done-when case: a page a host call is about to write was
/// evicted while its instance was idle. The call waits for the page
/// rather than faulting outside a fiber, and completes, its write
/// landing, once the page is back.
#[test]
fn a_host_call_writing_into_an_evicted_page_completes_once_it_is_back() {
    let mut harness = Harness::new();
    let head = harness.resident(1, 0x10);
    let tail = harness.resident(2, 0x20);
    harness.evict(tail).expect("evict the page the call writes");
    let residency = residency(&harness.handle);

    // Eight bytes either side of the page boundary.
    let range = VirtRange::new(VirtAddr::new(tail.raw() - 8), 16);
    let mut call = Box::pin(residency.with_present(range, || {
        copy_into(&[head, tail], 0xee);
        "written"
    }));
    assert!(
        poll(&mut call).is_none(),
        "the call waits for the page instead of touching it"
    );
    assert!(
        harness.index_of(tail).is_some(),
        "nothing has read the page back yet"
    );

    harness.serve();
    assert_eq!(poll(&mut call), Some(Ok("written")));
    let mut expected = pattern(0x20);
    expected[0] = 0xee;
    assert_eq!(
        harness.contents(tail),
        expected,
        "the page came back with its bytes, and the call's write landed on it"
    );
}

/// A page brought back for one check may be taken again before the
/// waiter is next polled; the access runs only after a check that finds
/// every page present, in its own poll.
#[test]
fn a_page_taken_again_before_the_access_runs_is_brought_back_again() {
    let mut harness = Harness::new();
    let page = harness.resident(1, 0x30);
    harness.evict(page).expect("evict");
    let residency = residency(&harness.handle);

    let mut call = Box::pin(residency.with_present(VirtRange::new(page, PAGE), || {
        copy_into(&[page], 0x31);
    }));
    assert!(poll(&mut call).is_none());
    harness.serve();
    // Back in, and out again before the waiter runs.
    harness.evict(page).expect("evict again");
    assert!(
        poll(&mut call).is_none(),
        "the re-check finds the page out and waits again"
    );
    harness.serve();
    assert_eq!(poll(&mut call), Some(Ok(())));
    assert_eq!(harness.contents(page)[0], 0x31);
    assert_eq!(harness.handle.stats().pages_in, 2);
}

/// A page that cannot be read back fails the access, which never runs:
/// the caller traps its guest rather than copying memory that is gone.
#[test]
fn a_page_that_cannot_come_back_fails_the_access_without_running_it() {
    let mut harness = Harness::new();
    let page = harness.resident(1, 0x40);
    harness.evict(page).expect("evict");
    harness.backend.fail_reads(1);
    let residency = residency(&harness.handle);

    let mut call = Box::pin(residency.with_present(VirtRange::new(page, PAGE), || {
        panic!("the access ran over a page that never came back")
    }));
    assert!(poll(&mut call).is_none());
    harness.serve();
    assert_eq!(poll(&mut call), Some(Err(SwapFaultError::Backend)));
}

/// Before the swap device comes up nothing can have been swapped out, so
/// an access runs at once without asking the address space anything.
#[test]
fn an_access_before_swap_comes_up_runs_at_once() {
    let residency = GuestResidency {
        hooks: &TEST_HOOKS,
        handle: Box::leak(Box::new(Once::new())),
    };
    let mut call =
        Box::pin(residency.with_present(VirtRange::new(VirtAddr::new(PAGE), PAGE), || "ran"));
    assert_eq!(poll(&mut call), Some(Ok("ran")));
}

/// The stream path: a page of a guest's read buffer is swapped out when
/// a host producer is about to deliver into it. The runtime asks the
/// residency hook in the poll that would make the copy; the hook hands
/// back the fault-in instead of letting the copy through, and once that
/// has resolved the same check lets the delivery go ahead.
#[cfg(feature = "wasmtime-runtime")]
#[test]
fn a_stream_delivery_into_an_evicted_read_buffer_waits_for_the_fault_in() {
    use core::ptr::NonNull;

    use wasmtime::component::GuestMemoryResidency;

    use crate::wasmtime_adapter::residency::SwapResidencyHook;

    let mut harness = Harness::new();
    let buffer = harness.resident(3, 0x50);
    let tail = harness.resident(4, 0x60);
    harness
        .evict(tail)
        .expect("evict the second page of the read buffer");
    let mut hook = SwapResidencyHook::new(residency(&harness.handle));

    // The guest's read buffer as the runtime names it: host addresses,
    // two pages long. The fake address space is never dereferenced.
    let read_buffer = NonNull::slice_from_raw_parts(
        NonNull::new(buffer.raw() as *mut u8).expect("a page is never at address zero"),
        2 * PAGE,
    );
    let mut wait = Box::pin(
        hook.wait_until_resident(read_buffer)
            .expect("a page of the read buffer is out, so the delivery must wait"),
    );
    assert!(
        poll(&mut wait).is_none(),
        "the fault-in waits for the device"
    );
    harness.serve();
    assert_eq!(poll(&mut wait), Some(Ok(())));
    assert!(
        hook.wait_until_resident(read_buffer).is_none(),
        "every page of the buffer is back, so the delivery goes ahead in this poll"
    );
    assert_eq!(harness.contents(tail), pattern(0x60));
}
