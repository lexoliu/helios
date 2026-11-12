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
use crate::{InstanceActivity, ProcessorTurn, RegisteredInstance, StoreTurns};

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
    /// The instance every page belongs to, and the registry that knows
    /// whether it is on a processor: a detach checks it at the moment it
    /// happens.
    owner: Option<(InstanceRegistry, InstanceId)>,
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
            if let Some((registry, id)) = &space.owner {
                assert!(
                    registry.idle_instances(u64::MAX, 0).contains(id),
                    "page {:#x} detached while its instance is on a processor",
                    addr.raw()
                );
            }
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
    swap_in_page: |addr, expected, context, fill| {
        with_space(|space| {
            if space.reinstatements_to_refuse > 0 {
                space.reinstatements_to_refuse -= 1;
                return Err(AddressSpaceError::OutOfFrames);
            }
            let mut bytes = vec![0_u8; PAGE];
            fill(context, &mut bytes);
            let page = space
                .pages
                .get_mut(&addr.raw())
                .ok_or(AddressSpaceError::NotSwapped)?;
            let FakePage::Swapped(token) = *page else {
                return Err(AddressSpaceError::NotSwapped);
            };
            if token != expected {
                return Err(AddressSpaceError::SwapTokenChanged);
            }
            *page = FakePage::Present(bytes);
            Ok(())
        })
    },
    swapped_token: |addr| {
        with_space(|space| match space.pages.get(&addr.page_floor().raw()) {
            Some(FakePage::Swapped(token)) => Some(*token),
            _ => None,
        })
    },
    // Every resident page belongs to the one instance, and every one is
    // cold: the pass takes whatever its plan has room for.
    scan_committed_pages: |_, context, visit| {
        let resident: Vec<VirtAddr> = with_space(|space| {
            space
                .pages
                .iter()
                .filter(|(_, page)| matches!(page, FakePage::Present(_)))
                .map(|(addr, _)| VirtAddr::new(*addr))
                .collect()
        });
        let mut visited = 0;
        for addr in resident {
            visited += 1;
            if !visit(
                context,
                addr,
                PageFlags::READ | PageFlags::WRITE,
                PageAge::Cold,
            ) {
                break;
            }
        }
        visited
    },
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
    /// The address of a page whose range the next successful read
    /// decommits while it is in flight: the page-table entry is gone and
    /// its token is queued for the drain by the time the read lands.
    decommit_on_read: Option<usize>,
    /// A store whose turn begins once this many more writes have landed:
    /// another processor putting the instance on while the pass awaits
    /// the device.
    turn_after_writes: Option<(u32, &'static StoreTurns)>,
    /// The turn that began, until the test ends it.
    open_turn: Option<ProcessorTurn<'static>>,
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

    fn decommit_during_next_read(&self, addr: VirtAddr) {
        self.state.lock().unwrap().decommit_on_read = Some(addr.raw());
    }

    fn begin_turn_after_writes(&self, writes: u32, turns: &'static StoreTurns) {
        self.state.lock().unwrap().turn_after_writes = Some((writes, turns));
    }

    fn end_turn(&self, now_nanos: u64) {
        let turn = self
            .state
            .lock()
            .unwrap()
            .open_turn
            .take()
            .expect("a turn began during the pass");
        let _ = turn.end(now_nanos);
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
        if let Some((writes, turns)) = state.turn_after_writes.as_mut() {
            *writes -= 1;
            if *writes == 0 {
                let turns = *turns;
                state.turn_after_writes = None;
                state.open_turn = Some(turns.begin(1));
            }
        }
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
        if let Some(addr) = state.decommit_on_read.take() {
            with_space(|space| {
                if let Some(FakePage::Swapped(token)) = space.pages.remove(&addr) {
                    space.orphans.push(token);
                }
            });
        }
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
    /// The instance every resident page belongs to. It has no store, so
    /// it is off every processor until a test gives it one.
    instance: RegisteredInstance,
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
        let registry = InstanceRegistry::new();
        let instance = registry.register("evicted", 0);
        with_space(|space| space.owner = Some((registry.clone(), instance.id())));
        let service = SwapService {
            backend: backend.clone(),
            handle: handle.clone(),
            hooks: &TEST_HOOKS,
            registry,
            cpu: TestCpu::with_entropy(0),
            slots: SlotAllocator::new(),
            disk: DiskEntries::new(),
            deferred: Vec::new(),
            page: vec![0_u8; PAGE].into_boxed_slice(),
            report: SwapReport::default(),
        };
        Self {
            service,
            handle,
            backend,
            instance,
        }
    }

    fn evict_instance(&mut self, budget: usize) -> usize {
        let id = self.instance.id();
        block_on(self.service.evict_owner(id, budget, EvictionAge::Any))
    }

    fn is_resident(&self, addr: VirtAddr) -> bool {
        with_space(|space| matches!(space.pages[&addr.raw()], FakePage::Present(_)))
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
        let id = self.instance.id();
        block_on(self.service.swap_out(id, addr))
    }

    /// The index the page-table entry carries, if the page is swapped.
    fn index_of(&self, addr: VirtAddr) -> Option<SwapToken> {
        (TEST_HOOKS.swapped_token)(addr)
    }

    /// What the shared slot table says about the slot a token names.
    fn slot(&self, token: SwapToken) -> SlotState {
        self.handle
            .shared
            .slots
            .state(SlotIndex::from_token(token).expect("a token within the table"))
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

/// An instance that begins a turn while the pass is waiting on the
/// device keeps every page the pass had planned to take: nothing leaves
/// memory while it runs, and the pass takes the rest once it is off
/// again.
#[test]
fn an_instance_that_starts_running_mid_pass_keeps_its_pages() {
    let mut harness = Harness::new();
    let pages: Vec<VirtAddr> = (1..=4)
        .map(|number| harness.resident(number, number as u8))
        .collect();
    let activity = InstanceActivity::new(harness.instance.clone(), 0);
    let turns: &'static StoreTurns = Box::leak(Box::new(activity.turns()));
    harness.backend.begin_turn_after_writes(1, turns);

    assert_eq!(
        harness.evict_instance(pages.len() * PAGE),
        PAGE,
        "the page whose write was in flight is out, and nothing after it"
    );
    assert!(!harness.is_resident(pages[0]));
    for &addr in &pages[1..] {
        assert!(
            harness.is_resident(addr),
            "page {:#x} left memory while its instance ran",
            addr.raw()
        );
    }

    harness.backend.end_turn(2);
    assert_eq!(harness.evict_instance(pages.len() * PAGE), 3 * PAGE);
    for (number, &addr) in pages.iter().enumerate() {
        harness.fault(addr).expect("fault the page back");
        assert_eq!(harness.contents(addr), pattern(number as u8 + 1));
    }
    drop(activity);
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

#[test]
fn a_stale_token_is_refused_and_the_fault_reads_the_entry_again() {
    let mut harness = Harness::new();
    let a = harness.resident(1, 0x11);
    let b = harness.resident(2, 0x22);
    let c = harness.resident(3, 0x33);
    harness.evict(a).expect("evict a");
    let stale = harness.index_of(a).expect("a is swapped");
    harness.evict(b).expect("evict b");
    harness.fault(a).expect("a comes back and frees its slot");
    harness.evict(c).expect("evict c");
    assert_eq!(
        harness.index_of(c),
        Some(stale),
        "the slot a freed is reused for another page"
    );
    harness.evict(a).expect("evict a again");
    let current = harness.index_of(a).expect("a is swapped again");
    assert_ne!(current, stale, "a went out under another slot");

    // A fault that read `stale` for `a` before all of that happened.
    let landed = block_on(harness.service.swap_in_token(a, stale));
    assert!(
        matches!(landed, Ok(Landed::Stale)),
        "the address space refuses a token that is not the entry's"
    );
    assert_eq!(
        harness.index_of(c),
        Some(stale),
        "the slot's own page is where it was"
    );
    assert!(matches!(harness.slot(stale), SlotState::Disk(_)));

    harness.fault(a).expect("the retry reads the entry again");
    assert_eq!(
        harness.contents(a),
        pattern(0x11),
        "a reads back its own bytes"
    );
    harness.fault(c).expect("c is unharmed");
    assert_eq!(harness.contents(c), pattern(0x33));
    harness.fault(b).expect("b is unharmed");
    assert_eq!(harness.contents(b), pattern(0x22));
}

#[test]
fn a_read_that_lands_after_the_range_was_decommitted_leaves_the_slot_for_the_drain() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0x77);
    harness.evict(addr).expect("evict");
    let index = harness.index_of(addr).expect("swapped");
    assert_eq!(harness.handle.stats().used_bytes, PAGE as u64);

    harness.backend.decommit_during_next_read(addr);
    assert_eq!(
        harness.fault(addr),
        Ok(()),
        "a decommitted page is not a failed fault: the retried instruction classifies it afresh"
    );
    assert_eq!(harness.slot(index), SlotState::Vacant);
    assert_eq!(harness.handle.stats().pages_in, 0, "nothing was reinstated");
    assert_eq!(
        harness.handle.stats().used_bytes,
        0,
        "the extent was consumed by the read"
    );

    block_on(harness.service.release_orphaned_tokens());
    assert!(matches!(harness.slot(index), SlotState::Free { .. }));
    assert_eq!(
        harness.backend.released(),
        Vec::<u64>::new(),
        "the extent was consumed by the read and is not released again"
    );
    let next = harness.resident(2, 0x78);
    harness.evict(next).expect("evict next");
    assert_eq!(harness.index_of(next), Some(index), "the slot is reused");
}

#[test]
fn an_orphan_that_meets_a_claimed_slot_is_kept_until_the_claim_resolves() {
    let mut harness = Harness::new();
    let addr = harness.resident(1, 0x55);
    harness.evict(addr).expect("evict");
    let index = harness.index_of(addr).expect("swapped");
    let state = harness.slot(index);
    let slot = SlotIndex::from_token(index).expect("in the table");
    let claim = harness
        .handle
        .shared
        .slots
        .claim(slot, state)
        .expect("nothing else holds the slot");

    // The range is decommitted while a fault holds the slot.
    with_space(|space| {
        space.pages.remove(&addr.raw());
    });
    harness.teardown(index);
    assert_eq!(harness.slot(index), SlotState::Claimed);
    assert_eq!(harness.backend.released(), Vec::<u64>::new());

    // The fault's reinstatement was refused, the entry being gone, and it
    // put the word back.
    harness.handle.shared.slots.publish(claim, state);
    block_on(harness.service.release_orphaned_tokens());
    assert_eq!(
        harness.backend.released(),
        vec![1],
        "the kept orphan is released on the next pass, with no new one drained"
    );
    assert!(matches!(harness.slot(index), SlotState::Free { .. }));
    assert_eq!(harness.handle.stats().used_bytes, 0);
}
