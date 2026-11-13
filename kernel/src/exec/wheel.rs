//! One processor's timing wheel, linked through the sleeps it holds.
//!
//! This is Tokio's hierarchical wheel (`runtime/time/wheel`, 1.54) on
//! Helios's 50 µs quantum: [`TIMER_WHEEL_LEVELS`] levels of
//! [`TIMER_WHEEL_SLOTS`] slots, each slot an intrusive doubly linked list
//! of [`SleepNode`]s, with one occupancy word per level so the next
//! expiration is found by `rotate_right` and `trailing_zeros` rather than
//! by a scan. A slot is processed whole: every node in it is either due,
//! and moves to the `pending` list, or is re-filed one level down. Each
//! node is re-filed at most `TIMER_WHEEL_LEVELS - 1` times in its life,
//! except a deadline past the top level's horizon, which the top level
//! carries round once per rotation.
//!
//! The wheel allocates nothing and owns no node. A node is the
//! [`SleepNode`] inside a pinned `Sleep`, and the wheel links that memory
//! in place, so registering cannot fail for capacity: there is nothing to
//! run out of. `pending` is wheel-owned, so a node that is due but not yet
//! fired can still be unlinked by its sleep's drop.
//!
//! Every method takes `&mut self`, and the only `&mut TimingWheel` there
//! is comes from the [`WheelLock`] around it in `TimerShared`: holding the
//! wheel *is* holding the wheel lock. A node records the lock of the wheel
//! it is linked in, its *home*, which changes only when a departing
//! processor hands its wheel to another ([`TimingWheel::move_to`]). The
//! concurrency contract, the node states and the invariants the `unsafe`
//! code below names (lifetime, lock, home, last access) are stated once,
//! in `timer`'s module docs.

use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use core::task::Waker;

use atomic_waker::AtomicWaker;
use spin::Mutex;

/// The lock around one processor's wheel. Every one lives in a timer base
/// that is kept for the life of the kernel, so a node's home is `'static`.
pub(super) type WheelLock = Mutex<TimingWheel>;

/// Bits of a tick each level resolves: 64 slots, so one `u64` holds a
/// level's occupancy (Tokio `wheel/mod.rs`, `BITS_PER_LEVEL`).
const LEVEL_BITS: u32 = 6;

/// Slots per level (Tokio `wheel/level.rs`, `LEVEL_MULT`).
pub(super) const TIMER_WHEEL_SLOTS: usize = 1 << LEVEL_BITS;

/// Levels in the wheel (Tokio `wheel/mod.rs`, `NUM_LEVELS`): 64⁶ quanta
/// of 50 µs, about 39.8 days, before a deadline is carried round the top
/// level.
pub(super) const TIMER_WHEEL_LEVELS: usize = 6;

// One occupancy word per level.
const _: () = assert!(TIMER_WHEEL_SLOTS == u64::BITS as usize);

/// Ticks the wheel spans before a deadline is filed in the top level and
/// carried round it (Tokio `wheel/mod.rs`, `MAX_DURATION`).
const MAX_DURATION: u64 = 1 << (LEVEL_BITS as usize * TIMER_WHEEL_LEVELS);

/// Where a sleep is in its life. Stored in [`SleepNode::state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum NodeState {
    /// On no list, and no processor but its holder has its address.
    Idle = 0,
    /// Linked in a slot list or in `pending`. Entered only under the lock.
    Registered = 1,
    /// Off the wheel; its firer, still holding the lock, is taking its
    /// waker. Exists only while that lock is held.
    Firing = 2,
    /// Off the wheel for good: fired, or deregistered. Stored only under
    /// the lock, as the last access of whoever took it off.
    Done = 3,
}

impl NodeState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Idle,
            1 => Self::Registered,
            2 => Self::Firing,
            3 => Self::Done,
            _ => panic!("sleep node state {value} was never stored"),
        }
    }

    /// Whether a poll that reads this state completes the sleep.
    pub(super) fn is_complete(self) -> bool {
        matches!(self, Self::Firing | Self::Done)
    }
}

/// A sleep's place in its timer's wheel, inside the sleep.
///
/// `links`, `deadline_tick` and `place` are touched only under the lock of
/// the node's home (the lock invariant). `home` is written only with that
/// lock held, and by a hand-over with the new home's lock held as well,
/// and read anywhere; `state` is written only under the home lock and read
/// anywhere; `waker` is registered by the poller and taken by the firer.
pub(super) struct SleepNode {
    links: UnsafeCell<Links>,
    /// The quantum tick the node is filed under.
    deadline_tick: UnsafeCell<u64>,
    /// The list the node is on, so an unlink needs no recomputation.
    place: UnsafeCell<Option<Place>>,
    /// The lock of the wheel the node is linked in. Null until the node
    /// is first registered; it then always names a `'static` lock.
    home: AtomicPtr<WheelLock>,
    state: AtomicU8,
    waker: AtomicWaker,
    /// A registered node's address is in the wheel.
    _pin: PhantomPinned,
}

// SAFETY: the cells are touched only by the holder of the home lock (the
// lock invariant), which `spin::Mutex` hands to one processor at a time
// and whose acquire orders each holder after the last. `home` and `state`
// are atomics and `AtomicWaker` is `Send + Sync`. A node moves between
// processors with its `Sleep`, which owns it.
unsafe impl Send for SleepNode {}
// SAFETY: as for `Send`: a shared node is read and written only through
// atomics, the `AtomicWaker`, or the cells under the home lock.
unsafe impl Sync for SleepNode {}

#[derive(Clone, Copy)]
struct Links {
    prev: Option<NonNull<SleepNode>>,
    next: Option<NonNull<SleepNode>>,
}

impl Links {
    const UNLINKED: Self = Self {
        prev: None,
        next: None,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Slot { level: u8, slot: u8 },
    Pending,
}

impl SleepNode {
    pub(super) const fn new() -> Self {
        Self {
            links: UnsafeCell::new(Links::UNLINKED),
            deadline_tick: UnsafeCell::new(0),
            place: UnsafeCell::new(None),
            home: AtomicPtr::new(ptr::null_mut()),
            state: AtomicU8::new(NodeState::Idle as u8),
            waker: AtomicWaker::new(),
            _pin: PhantomPinned,
        }
    }

    /// The node's state. `Acquire`: reading [`NodeState::Done`]
    /// synchronizes with the `Release` store its remover made as its last
    /// access, so everything that remover did happens before this reader
    /// goes on, and frees the node.
    pub(super) fn state(&self) -> NodeState {
        NodeState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Registers the waker a fire will wake. Lock-free; the poller calls
    /// it before it re-tests the state (`AtomicWaker`'s protocol).
    pub(super) fn register_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    /// Takes the node off whichever wheel it is on, unless a firer already
    /// has, and leaves it [`NodeState::Done`] (Linux `lock_hrtimer_base`).
    ///
    /// The home read before the lock may be stale, because a hand-over
    /// re-points it while this caller spins. The re-read under the lock is
    /// not: a home changes only while its lock is held, and this caller now
    /// holds it, so a node that still names it is on that wheel until the
    /// caller lets go. A node a hand-over moved names the new home, and the
    /// caller follows it there.
    ///
    /// # Safety
    ///
    /// The node has been registered (its state is not
    /// [`NodeState::Idle`]) and is live: the caller is its holder.
    pub(super) unsafe fn deregister(node: NonNull<Self>) {
        // SAFETY: the caller holds the node, so it is live.
        let live = unsafe { node.as_ref() };
        loop {
            let home = live.home();
            let mut wheel = home.lock();
            if ptr::eq(live.home(), home) {
                // SAFETY: the node is registered on the wheel this lock
                // guards, which it names as its home under that lock, and
                // the caller holds it.
                unsafe { wheel.deregister(node) };
                return;
            }
        }
    }

    /// The lock of the wheel the node is linked in, or was last linked in.
    ///
    /// `Relaxed`: the caller has read a registered state with `Acquire`,
    /// which the registrant's `Release` store made after it set the home,
    /// so the pointer is never null here; any later value is a lock that
    /// lives for the life of the kernel, and which one is current is
    /// decided only under it.
    fn home(&self) -> &'static WheelLock {
        let home = self.home.load(Ordering::Relaxed);
        // SAFETY: a non-null home is the address of a `WheelLock` inside a
        // timer base, and no base is ever freed.
        unsafe { home.as_ref() }.expect("a sleep node that was never registered has no home")
    }

    fn set_home(&self, home: &'static WheelLock) {
        self.home
            .store(ptr::from_ref(home).cast_mut(), Ordering::Relaxed);
    }

    fn store_state(&self, state: NodeState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// The node's links.
    ///
    /// # Safety
    ///
    /// `node` is live. The caller holds the wheel lock before it reads or
    /// writes through the pointer.
    unsafe fn links(node: NonNull<Self>) -> *mut Links {
        // SAFETY: `node` is live (the caller's contract); this projects
        // the field without creating a reference to the node.
        unsafe { UnsafeCell::raw_get(&raw const (*node.as_ptr()).links) }
    }

    /// The node's filed tick. Safety as for [`Self::links`].
    unsafe fn deadline_tick(node: NonNull<Self>) -> *mut u64 {
        // SAFETY: as in `links`.
        unsafe { UnsafeCell::raw_get(&raw const (*node.as_ptr()).deadline_tick) }
    }

    /// The list the node is on. Safety as for [`Self::links`].
    unsafe fn place(node: NonNull<Self>) -> *mut Option<Place> {
        // SAFETY: as in `links`.
        unsafe { UnsafeCell::raw_get(&raw const (*node.as_ptr()).place) }
    }

    /// Fires a node the wheel has just taken off `pending` and returns its
    /// waker, to be woken once the lock is released.
    ///
    /// `Firing` is stored before the waker is taken, as `AtomicWaker`
    /// requires of every waker: a poll that registers a new waker
    /// concurrently either has it taken here, or sees the `WAKING` bit
    /// this take sets, which orders it after the `Firing` store, so its
    /// re-test reads a complete state. `Done` is stored last, and nothing
    /// here touches the node after it.
    ///
    /// # Safety
    ///
    /// The caller holds the wheel lock and has unlinked `node`, which was
    /// [`NodeState::Registered`] and so live (the lifetime invariant): its
    /// sleep's drop either has not begun or is waiting on the lock.
    unsafe fn fire(node: NonNull<Self>) -> Option<Waker> {
        // SAFETY: the node is live until the `Done` store below, by the
        // caller's contract; a dropper that reads `Firing` takes the lock
        // this caller holds.
        let live = unsafe { node.as_ref() };
        debug_assert_eq!(live.state(), NodeState::Registered);
        live.store_state(NodeState::Firing);
        let waker = live.waker.take();
        // SAFETY: still live, as above. The `Release` store is the last
        // access (the last-access invariant): once a dropper's `Acquire`
        // load reads it, the node may be freed.
        let state = unsafe { &raw const (*node.as_ptr()).state };
        // SAFETY: `state` points into the live node; the store is the
        // final use of the pointer.
        unsafe { (*state).store(NodeState::Done as u8, Ordering::Release) };
        waker
    }
}

/// An intrusive doubly linked list of nodes through their [`Links`].
struct List {
    head: Option<NonNull<SleepNode>>,
    tail: Option<NonNull<SleepNode>>,
}

impl List {
    const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// # Safety
    ///
    /// The caller holds the wheel lock, `node` is live and on no list, and
    /// every node on this list is live.
    unsafe fn push_front(&mut self, node: NonNull<SleepNode>) {
        // SAFETY: `node` and the current head are live and the lock is
        // held (the caller's contract), so their links are this caller's
        // to write.
        unsafe {
            *SleepNode::links(node) = Links {
                prev: None,
                next: self.head,
            };
            match self.head {
                Some(head) => (*SleepNode::links(head)).prev = Some(node),
                None => self.tail = Some(node),
            }
        }
        self.head = Some(node);
    }

    /// # Safety
    ///
    /// The caller holds the wheel lock, `node` is on this list, and every
    /// node on it is live.
    unsafe fn remove(&mut self, node: NonNull<SleepNode>) {
        // SAFETY: `node` and its neighbours are on this list and live, and
        // the lock is held (the caller's contract).
        unsafe {
            let Links { prev, next } = *SleepNode::links(node);
            match prev {
                Some(prev) => (*SleepNode::links(prev)).next = next,
                None => {
                    debug_assert_eq!(self.head, Some(node));
                    self.head = next;
                }
            }
            match next {
                Some(next) => (*SleepNode::links(next)).prev = prev,
                None => {
                    debug_assert_eq!(self.tail, Some(node));
                    self.tail = prev;
                }
            }
            *SleepNode::links(node) = Links::UNLINKED;
        }
    }

    /// The oldest node pushed, unlinked.
    ///
    /// # Safety
    ///
    /// The caller holds the wheel lock and every node on this list is live.
    unsafe fn pop_back(&mut self) -> Option<NonNull<SleepNode>> {
        let tail = self.tail?;
        // SAFETY: `tail` is on this list; the rest is the caller's contract.
        unsafe { self.remove(tail) };
        Some(tail)
    }

    /// # Safety
    ///
    /// The caller holds the wheel lock and every node on this list is live.
    unsafe fn len(&self) -> usize {
        let mut count = 0;
        let mut node = self.head;
        while let Some(current) = node {
            count += 1;
            // SAFETY: `current` is on this list and live (the caller's
            // contract), and the lock is held.
            node = unsafe { (*SleepNode::links(current)).next };
        }
        count
    }
}

/// What one batch of a hand-over ([`TimingWheel::move_to`]) moved.
pub(super) struct MovedNodes {
    /// The earliest tick among the nodes moved, if any were.
    pub(super) earliest_tick: Option<u64>,
    /// Whether the wheel moved from is now empty.
    pub(super) emptied: bool,
}

/// When a level's next occupied slot must be processed.
struct Expiration {
    level: usize,
    slot: usize,
    /// The quantum tick at which the slot is processed: its start.
    deadline: u64,
}

/// The wheel itself. `#[repr(C)]` keeps the words every lock holder
/// touches (`elapsed`, `pending`, the occupancy words) together at the
/// front, on the lock's own lines; the slot heads follow.
#[repr(C)]
pub(super) struct TimingWheel {
    /// The quantum tick the wheel has been advanced to.
    elapsed: u64,
    /// Due nodes not yet fired, newest at the front.
    pending: List,
    /// Bit `s` of word `l` is set while slot `s` of level `l` is non-empty.
    occupied: [u64; TIMER_WHEEL_LEVELS],
    slots: [[List; TIMER_WHEEL_SLOTS]; TIMER_WHEEL_LEVELS],
}

// SAFETY: the wheel owns no node. The pointers it holds are dereferenced
// only through `&mut TimingWheel`, which the `spin::Mutex` around it hands
// to one processor at a time (the lock invariant), and the lifetime
// invariant keeps every linked node alive until its sleep's drop has
// unlinked it under that lock.
unsafe impl Send for TimingWheel {}

impl TimingWheel {
    /// An empty wheel advanced to `elapsed`, the owner's current quantum.
    pub(super) const fn new(elapsed: u64) -> Self {
        Self {
            elapsed,
            pending: List::new(),
            occupied: [0; TIMER_WHEEL_LEVELS],
            slots: [const { [const { List::new() }; TIMER_WHEEL_SLOTS] }; TIMER_WHEEL_LEVELS],
        }
    }

    /// Links `node` under `deadline_tick`, makes `home` its home and marks
    /// it [`NodeState::Registered`]. A tick the wheel has already passed
    /// goes straight to `pending`, to fire on the owner's next pass.
    ///
    /// # Safety
    ///
    /// `home` is the lock this `&mut self` came from. `node` is
    /// [`NodeState::Idle`], pinned, and is deregistered under its home's
    /// lock before its memory is released (the lifetime invariant); the
    /// holder of its sleep is the caller, so nothing else touches it.
    pub(super) unsafe fn register(
        &mut self,
        home: &'static WheelLock,
        node: NonNull<SleepNode>,
        deadline_tick: u64,
    ) {
        // SAFETY: the node is live and pinned (the caller's contract), and
        // `&mut self` is the wheel lock.
        unsafe {
            debug_assert_eq!(node.as_ref().state(), NodeState::Idle);
            *SleepNode::deadline_tick(node) = deadline_tick;
            self.insert(node, deadline_tick);
            node.as_ref().set_home(home);
            // `Release`: a reader that sees `Registered` sees the home.
            node.as_ref().store_state(NodeState::Registered);
        }
    }

    /// Moves up to `limit` registered nodes from this wheel to `to`, whose
    /// lock is `to_home`, re-filing each under its own tick against `to`'s
    /// clock and re-pointing its home: one batch of a departing
    /// processor's hand-over (Linux `migrate_hrtimer_list`).
    ///
    /// A moved node stays [`NodeState::Registered`] throughout, with its
    /// waker in place, and is off every list only inside this call, while
    /// the caller holds both locks: a dropper looking for it waits on this
    /// wheel's lock and then follows its home to `to`, and no firer can
    /// reach it until it is on `to`'s lists.
    ///
    /// # Safety
    ///
    /// `&mut self` and `to` are the guards of two different locks, both
    /// held by the caller, and `to_home` is the lock `to` came from.
    pub(super) unsafe fn move_to(
        &mut self,
        to: &mut TimingWheel,
        to_home: &'static WheelLock,
        limit: usize,
    ) -> MovedNodes {
        let mut moved = MovedNodes {
            earliest_tick: None,
            emptied: false,
        };
        for _ in 0..limit {
            let Some(node) = self.pop_any() else {
                moved.emptied = true;
                return moved;
            };
            // SAFETY: `pop_any` has just unlinked `node` under this lock, so
            // it was registered and is live (the lifetime invariant); `to`'s
            // lock is held too, so filing it there and naming that lock its
            // home is this caller's to do.
            unsafe {
                let tick = *SleepNode::deadline_tick(node);
                to.insert(node, tick);
                node.as_ref().set_home(to_home);
                moved.earliest_tick = Some(moved.earliest_tick.map_or(tick, |t| t.min(tick)));
            }
        }
        moved.emptied = self.is_empty();
        moved
    }

    /// Moves an empty wheel's clock to `elapsed`, for a processor coming
    /// back online: its wheel was emptied when it went offline, and walking
    /// every quantum it was away for would find nothing.
    pub(super) fn restart(&mut self, elapsed: u64) {
        assert!(
            self.is_empty(),
            "a timer base comes online with sleeps still on its wheel"
        );
        self.elapsed = elapsed;
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.occupied.iter().all(|&word| word == 0)
    }

    /// Takes `node` off the wheel if it is still on it, and marks it
    /// [`NodeState::Done`]. A node a firer completed while this caller
    /// waited for the lock is already `Done` and is left alone.
    ///
    /// # Safety
    ///
    /// `node` was registered on this wheel and is live (the lifetime
    /// invariant: the caller is its holder).
    pub(super) unsafe fn deregister(&mut self, node: NonNull<SleepNode>) {
        // SAFETY: the caller holds the node, so it is live; `&mut self` is
        // the wheel lock.
        let live = unsafe { node.as_ref() };
        match live.state() {
            NodeState::Registered => {
                // SAFETY: a registered node is on the list its `place`
                // names, every node on it is live, and the lock is held.
                unsafe { self.unlink(node) };
                live.store_state(NodeState::Done);
            }
            NodeState::Done => {}
            state => panic!("deregistering a sleep node in state {state:?} under the wheel lock"),
        }
    }

    /// Fires the next node due at `now_tick`, if any: unlinks it, marks it
    /// [`NodeState::Done`] and returns its waker, if one was registered,
    /// for the caller to wake once the lock is released (Tokio
    /// `Wheel::poll` and `StateCell::fire`).
    pub(super) fn fire_next(&mut self, now_tick: u64) -> Option<Option<Waker>> {
        let node = self.poll(now_tick)?;
        // SAFETY: `poll` has just unlinked `node` from `pending` under this
        // lock, so it was registered and is live (the lifetime invariant).
        Some(unsafe { SleepNode::fire(node) })
    }

    /// The quantum tick at which the wheel next has work: `elapsed` while
    /// a node is pending, else the start of the earliest occupied slot.
    /// No published deadline is ever later than a node's tick.
    pub(super) fn next_expiration_tick(&self) -> Option<u64> {
        self.next_expiration().map(|expiration| expiration.deadline)
    }

    /// The number of registered nodes, by walking every list.
    pub(super) fn registered(&self) -> usize {
        let lists = core::iter::once(&self.pending).chain(self.slots.iter().flatten());
        // SAFETY: every node on a wheel list is live (the lifetime
        // invariant) and `&self` is under the wheel lock.
        lists.map(|list| unsafe { list.len() }).sum()
    }

    /// Advances the wheel to `now_tick`, moving every slot that comes due
    /// on the way, and returns the oldest pending node, unlinked (Tokio
    /// `Wheel::poll`).
    fn poll(&mut self, now_tick: u64) -> Option<NonNull<SleepNode>> {
        // Only the owner advances the wheel, from its own clock, which is
        // monotonic. Tokio clamps here all the same (`process_at_time`),
        // because a virtualized clock has been seen to step back; a wheel
        // never moves backwards.
        let now_tick = now_tick.max(self.elapsed);
        loop {
            if let Some(node) = self.pop_pending() {
                return Some(node);
            }
            match self.next_expiration() {
                Some(expiration) if expiration.deadline <= now_tick => {
                    self.process_expiration(&expiration);
                    self.elapsed = expiration.deadline;
                }
                _ => {
                    self.elapsed = now_tick;
                    return None;
                }
            }
        }
    }

    fn next_expiration(&self) -> Option<Expiration> {
        if !self.pending.is_empty() {
            return Some(Expiration {
                level: 0,
                slot: 0,
                deadline: self.elapsed,
            });
        }
        (0..TIMER_WHEEL_LEVELS).find_map(|level| self.level_expiration(level))
    }

    /// The next slot of `level` to process (Tokio `Level::next_expiration`).
    fn level_expiration(&self, level: usize) -> Option<Expiration> {
        let occupied = self.occupied[level];
        if occupied == 0 {
            return None;
        }
        let now = self.elapsed;
        // Start the search one past the slot `now` is in: below the top
        // level that slot is always empty, and at the top level an entry
        // there is a whole rotation away.
        let now_slot = ((now / slot_range(level)) % TIMER_WHEEL_SLOTS as u64) as u32 + 1;
        let slot = (occupied.rotate_right(now_slot).trailing_zeros() + now_slot) as usize
            % TIMER_WHEEL_SLOTS;
        let level_start = now & !(level_range(level) - 1);
        let mut deadline = level_start + slot as u64 * slot_range(level);
        if deadline <= now {
            // Only the top level wraps: a deadline past the horizon is
            // carried round it.
            debug_assert_eq!(level, TIMER_WHEEL_LEVELS - 1);
            deadline = deadline.saturating_add(level_range(level));
        }
        Some(Expiration {
            level,
            slot,
            deadline,
        })
    }

    /// Takes the whole slot first, because a node carried round the top
    /// level is re-filed into the same slot (Tokio `process_expiration`),
    /// then moves each node to `pending` or one level down.
    fn process_expiration(&mut self, expiration: &Expiration) {
        self.occupied[expiration.level] &= !occupied_bit(expiration.slot);
        let mut taken = core::mem::replace(
            &mut self.slots[expiration.level][expiration.slot],
            List::new(),
        );
        // SAFETY: every node on the taken list was on a wheel list and is
        // live (the lifetime invariant), and `&mut self` is the lock, held
        // throughout, so no dropper can look for a node while it is off
        // every list here.
        unsafe {
            while let Some(node) = taken.pop_back() {
                let when = *SleepNode::deadline_tick(node);
                if when <= expiration.deadline {
                    self.push_pending(node);
                } else {
                    self.file(node, level_for(expiration.deadline, when), when);
                }
            }
        }
    }

    /// Files `node` under `when`: in `pending` when the wheel has already
    /// passed it, else in the level [`level_for`] picks. The processors
    /// share one timebase, so a passed tick is a registrant whose clock
    /// read trails the owner's latest, or a handed-over node that is
    /// already due on its new wheel.
    ///
    /// # Safety
    ///
    /// The lock is held and `node` is live and on no list.
    unsafe fn insert(&mut self, node: NonNull<SleepNode>, when: u64) {
        // SAFETY: the caller's contract.
        unsafe {
            if when <= self.elapsed {
                self.push_pending(node);
            } else {
                self.file(node, level_for(self.elapsed, when), when);
            }
        }
    }

    /// Any registered node, unlinked: the oldest pending one, else the
    /// oldest in the first occupied slot of the lowest occupied level.
    fn pop_any(&mut self) -> Option<NonNull<SleepNode>> {
        if let Some(node) = self.pop_pending() {
            return Some(node);
        }
        let level = self.occupied.iter().position(|&word| word != 0)?;
        let slot = self.occupied[level].trailing_zeros() as usize;
        let list = &mut self.slots[level][slot];
        // SAFETY: every node on a wheel list is live (the lifetime
        // invariant) and `&mut self` is the lock.
        unsafe {
            let node = list
                .pop_back()
                .expect("an occupied wheel slot holds a node");
            if list.is_empty() {
                self.occupied[level] &= !occupied_bit(slot);
            }
            *SleepNode::place(node) = None;
            Some(node)
        }
    }

    /// # Safety
    ///
    /// The lock is held and `node` is live and on no list.
    unsafe fn file(&mut self, node: NonNull<SleepNode>, level: usize, when: u64) {
        let slot = slot_for(when, level);
        // SAFETY: the caller's contract.
        unsafe {
            self.slots[level][slot].push_front(node);
            *SleepNode::place(node) = Some(Place::Slot {
                level: level as u8,
                slot: slot as u8,
            });
        }
        self.occupied[level] |= occupied_bit(slot);
    }

    /// # Safety
    ///
    /// The lock is held and `node` is live and on no list.
    unsafe fn push_pending(&mut self, node: NonNull<SleepNode>) {
        // SAFETY: the caller's contract.
        unsafe {
            self.pending.push_front(node);
            *SleepNode::place(node) = Some(Place::Pending);
        }
    }

    fn pop_pending(&mut self) -> Option<NonNull<SleepNode>> {
        // SAFETY: every node on `pending` is live (the lifetime invariant)
        // and `&mut self` is the lock.
        unsafe {
            let node = self.pending.pop_back()?;
            *SleepNode::place(node) = None;
            Some(node)
        }
    }

    /// # Safety
    ///
    /// The lock is held and `node` is live and on the list its `place`
    /// names.
    unsafe fn unlink(&mut self, node: NonNull<SleepNode>) {
        // SAFETY: the caller's contract; `place` is written only under the
        // lock, alongside the list it names.
        unsafe {
            match (*SleepNode::place(node)).take() {
                Some(Place::Pending) => self.pending.remove(node),
                Some(Place::Slot { level, slot }) => {
                    let (level, slot) = (usize::from(level), usize::from(slot));
                    let list = &mut self.slots[level][slot];
                    list.remove(node);
                    if list.is_empty() {
                        self.occupied[level] &= !occupied_bit(slot);
                    }
                }
                None => panic!("a registered sleep node is on no wheel list"),
            }
        }
    }
}

/// The occupancy bit of `slot` in its level's word.
fn occupied_bit(slot: usize) -> u64 {
    1 << slot
}

/// Ticks one slot of `level` spans.
fn slot_range(level: usize) -> u64 {
    1 << (LEVEL_BITS as usize * level)
}

/// Ticks one rotation of `level` spans.
fn level_range(level: usize) -> u64 {
    1 << (LEVEL_BITS as usize * (level + 1))
}

/// The slot of `level` that tick `when` falls in.
fn slot_for(when: u64, level: usize) -> usize {
    ((when >> (LEVEL_BITS as usize * level)) % TIMER_WHEEL_SLOTS as u64) as usize
}

/// The level a node due at `when` is filed in while the wheel stands at
/// `elapsed`: the highest level whose slot the two differ in, and the top
/// level for anything past the horizon (Tokio `wheel/mod.rs`, `level_for`).
fn level_for(elapsed: u64, when: u64) -> usize {
    const SLOT_MASK: u64 = (1 << LEVEL_BITS) - 1;
    // The low bits are masked in so the leading-zero count is capped.
    let masked = (elapsed ^ when) | SLOT_MASK;
    if masked >= MAX_DURATION {
        return TIMER_WHEEL_LEVELS - 1;
    }
    masked.ilog2() as usize / LEVEL_BITS as usize
}

#[cfg(test)]
mod tests {
    use core::pin::{Pin, pin};
    use core::ptr::{self, NonNull};

    use std::boxed::Box;

    use spin::Mutex;

    use super::{NodeState, SleepNode, TIMER_WHEEL_LEVELS, TimingWheel, WheelLock, level_for};

    fn node_ptr(node: Pin<&SleepNode>) -> NonNull<SleepNode> {
        NonNull::from(node.get_ref())
    }

    /// A wheel advanced to `elapsed` behind a lock that lives as long as a
    /// timer base's does.
    fn wheel_lock(elapsed: u64) -> &'static WheelLock {
        Box::leak(Box::new(Mutex::new(TimingWheel::new(elapsed))))
    }

    /// Registers `node` at `tick` on `wheel`, the guard of `home`.
    fn register(
        home: &'static WheelLock,
        wheel: &mut TimingWheel,
        node: Pin<&SleepNode>,
        tick: u64,
    ) {
        // SAFETY: `wheel` is `home`'s guard. The node is pinned in the
        // test's frame and idle, and the test fires or deregisters it, or
        // never touches the wheel again.
        unsafe { wheel.register(home, node_ptr(node), tick) };
    }

    /// The counterpart of a departing processor's hand-over: every
    /// registered node moves, whatever level or list it is on, under its
    /// own tick against the receiving wheel's clock, and names the new
    /// wheel as its home; the wheel it left has no expiration. A
    /// deregistered node is on no list, so it does not travel.
    #[test]
    fn moving_a_whole_wheel_takes_every_registered_node_and_forgets_its_deadline() {
        let from_home = wheel_lock(0);
        let to_home = wheel_lock(100);
        let due = pin!(SleepNode::new());
        let near = pin!(SleepNode::new());
        let far = pin!(SleepNode::new());
        let cancelled = pin!(SleepNode::new());
        {
            let mut from = from_home.lock();
            register(from_home, &mut from, due.as_ref(), 0);
            register(from_home, &mut from, near.as_ref(), 2);
            register(from_home, &mut from, far.as_ref(), 300);
            register(from_home, &mut from, cancelled.as_ref(), 4);
            // SAFETY: `cancelled` is pinned above and registered here.
            unsafe { from.deregister(node_ptr(cancelled.as_ref())) };
            assert_eq!(from.registered(), 3);
        }

        // One node per batch: each batch but the last leaves some behind.
        let mut batches = 0;
        let mut earliest = None;
        loop {
            let mut from = from_home.lock();
            let mut to = to_home.lock();
            // SAFETY: the two guards are of two different locks, both held,
            // and `to` came from `to_home`.
            let moved = unsafe { from.move_to(&mut to, to_home, 1) };
            batches += 1;
            if let Some(tick) = moved.earliest_tick {
                earliest = Some(earliest.map_or(tick, |earliest: u64| earliest.min(tick)));
            }
            if moved.emptied {
                assert_eq!(from.next_expiration_tick(), None);
                assert_eq!(from.registered(), 0);
                break;
            }
        }
        assert_eq!(batches, 3);
        assert_eq!(earliest, Some(0));

        for node in [&due, &near, &far] {
            assert_eq!(node.state(), NodeState::Registered);
            assert!(ptr::eq(node.home(), to_home));
        }
        assert_eq!(cancelled.state(), NodeState::Done);
        let mut to = to_home.lock();
        assert_eq!(to.registered(), 3);
        // The two already due at the receiving wheel's clock fire on its
        // next pass; the far one at its own tick.
        assert_eq!(to.next_expiration_tick(), Some(100));
        assert!(to.fire_next(100).is_some());
        assert!(to.fire_next(100).is_some());
        assert!(to.fire_next(299).is_none());
        assert!(to.fire_next(300).is_some());
        assert_eq!(to.registered(), 0);
        for node in [&due, &near, &far] {
            assert_eq!(node.state(), NodeState::Done);
        }
    }

    /// A node a hand-over moved while its dropper waited is deregistered
    /// from the wheel it moved to, by following its home.
    #[test]
    fn a_node_deregisters_from_the_wheel_it_was_moved_to() {
        let from_home = wheel_lock(0);
        let to_home = wheel_lock(0);
        let node = pin!(SleepNode::new());
        register(from_home, &mut from_home.lock(), node.as_ref(), 7);
        {
            let mut from = from_home.lock();
            let mut to = to_home.lock();
            // SAFETY: two different locks, both held; `to` is `to_home`'s.
            assert!(unsafe { from.move_to(&mut to, to_home, usize::MAX) }.emptied);
        }

        // SAFETY: the node is pinned above and registered.
        unsafe { SleepNode::deregister(node_ptr(node.as_ref())) };

        assert_eq!(node.state(), NodeState::Done);
        assert_eq!(to_home.lock().registered(), 0);
        assert_eq!(to_home.lock().next_expiration_tick(), None);
    }

    #[test]
    fn level_for_files_each_level_by_its_highest_differing_slot() {
        for tick in 1..64 {
            assert_eq!(level_for(0, tick), 0);
        }
        for level in 1..TIMER_WHEEL_LEVELS {
            let start = 1_u64 << (6 * level);
            assert_eq!(level_for(0, start), level);
            assert_eq!(level_for(0, start * 2 - 1), level);
        }
        assert_eq!(level_for(0, 1 << 36), TIMER_WHEEL_LEVELS - 1);
        assert_eq!(level_for(0, u64::MAX), TIMER_WHEEL_LEVELS - 1);
    }

    #[test]
    fn an_entry_expires_at_its_tick_and_not_before() {
        let home = wheel_lock(0);
        let mut wheel = home.lock();
        let node = pin!(SleepNode::new());
        register(home, &mut wheel, node.as_ref(), 3);

        assert!(wheel.fire_next(2).is_none());
        assert!(wheel.fire_next(3).is_some());
        assert_eq!(node.state(), NodeState::Done);
        assert!(wheel.fire_next(3).is_none());
        assert_eq!(wheel.registered(), 0);
    }

    #[test]
    fn a_higher_level_entry_cascades_down_and_expires_at_its_tick() {
        let home = wheel_lock(0);
        let mut wheel = home.lock();
        let node = pin!(SleepNode::new());
        register(home, &mut wheel, node.as_ref(), 300);
        // Level 1, slot 4: processed at its start, 256, and re-filed.
        assert_eq!(wheel.next_expiration_tick(), Some(256));

        assert!(wheel.fire_next(299).is_none());
        assert_eq!(wheel.registered(), 1);
        assert_eq!(wheel.next_expiration_tick(), Some(300));
        assert!(wheel.fire_next(300).is_some());
        assert_eq!(wheel.registered(), 0);
    }

    #[test]
    fn a_deregistered_entry_never_fires_and_leaves_its_slot_empty() {
        let home = wheel_lock(0);
        let mut wheel = home.lock();
        let node = pin!(SleepNode::new());
        register(home, &mut wheel, node.as_ref(), 2);
        // SAFETY: the node is pinned above and registered on this wheel.
        unsafe { wheel.deregister(node_ptr(node.as_ref())) };

        assert_eq!(node.state(), NodeState::Done);
        assert_eq!(wheel.next_expiration_tick(), None);
        assert!(wheel.fire_next(2).is_none());
        assert_eq!(wheel.registered(), 0);
    }

    #[test]
    fn the_next_expiration_moves_on_once_the_front_entry_is_deregistered() {
        let home = wheel_lock(0);
        let mut wheel = home.lock();
        let front = pin!(SleepNode::new());
        let back = pin!(SleepNode::new());
        register(home, &mut wheel, front.as_ref(), 2);
        register(home, &mut wheel, back.as_ref(), 4);
        assert_eq!(wheel.next_expiration_tick(), Some(2));

        // SAFETY: `front` is pinned above and registered on this wheel.
        unsafe { wheel.deregister(node_ptr(front.as_ref())) };

        assert_eq!(wheel.next_expiration_tick(), Some(4));
        assert!(wheel.fire_next(3).is_none());
        assert!(wheel.fire_next(4).is_some());
        assert_eq!(back.state(), NodeState::Done);
    }

    /// The next expiration of an entry on each of the six levels is the
    /// start of its slot, and it fires at its own tick after cascading
    /// down from there.
    #[test]
    fn the_next_expiration_is_found_on_every_level() {
        for level in 0..TIMER_WHEEL_LEVELS {
            let home = wheel_lock(0);
            let mut wheel = home.lock();
            let node = pin!(SleepNode::new());
            let slot_start = if level == 0 { 0 } else { 1_u64 << (6 * level) };
            let when = slot_start + 10;
            register(home, &mut wheel, node.as_ref(), when);

            let expected = if level == 0 { when } else { slot_start };
            assert_eq!(
                wheel.next_expiration_tick(),
                Some(expected),
                "level {level}"
            );
            assert!(wheel.fire_next(when - 1).is_none(), "level {level}");
            assert_eq!(wheel.next_expiration_tick(), Some(when), "level {level}");
            assert!(wheel.fire_next(when).is_some(), "level {level}");
            assert_eq!(wheel.next_expiration_tick(), None, "level {level}");
        }
    }

    /// A deadline beyond 64⁶ quanta is filed in the top level and carried
    /// round it once per rotation until it is within reach.
    #[test]
    fn a_deadline_past_the_horizon_wraps_the_top_level() {
        let home = wheel_lock(0);
        let mut wheel = home.lock();
        let node = pin!(SleepNode::new());
        let when = (1_u64 << 37) + 5;
        register(home, &mut wheel, node.as_ref(), when);

        assert_eq!(wheel.next_expiration_tick(), Some(1 << 36));
        assert!(wheel.fire_next(1 << 36).is_none());
        assert_eq!(wheel.next_expiration_tick(), Some(1 << 37));
        assert!(wheel.fire_next(1 << 37).is_none());
        assert_eq!(wheel.next_expiration_tick(), Some(when));
        assert!(wheel.fire_next(when).is_some());
        assert_eq!(wheel.registered(), 0);
    }

    /// A firer takes due nodes off `pending` one at a time and may release
    /// the lock between them; a node still on `pending` is unlinked by its
    /// drop and never fires.
    #[test]
    fn an_entry_cancelled_from_pending_while_a_batch_is_out_never_fires() {
        let home = wheel_lock(0);
        let mut wheel = home.lock();
        let first = pin!(SleepNode::new());
        let second = pin!(SleepNode::new());
        register(home, &mut wheel, first.as_ref(), 5);
        register(home, &mut wheel, second.as_ref(), 5);

        assert!(wheel.fire_next(5).is_some());
        // Exactly one of the two fired; the other is pending.
        let (fired, waiting) = if first.state() == NodeState::Done {
            (first.as_ref(), second.as_ref())
        } else {
            (second.as_ref(), first.as_ref())
        };
        assert_eq!(fired.state(), NodeState::Done);
        assert_eq!(waiting.state(), NodeState::Registered);
        assert_eq!(wheel.registered(), 1);

        // SAFETY: `waiting` is pinned above and registered on this wheel.
        unsafe { wheel.deregister(node_ptr(waiting)) };

        assert_eq!(wheel.registered(), 0);
        assert!(wheel.fire_next(5).is_none());
        assert_eq!(waiting.state(), NodeState::Done);
    }

    /// A registrant whose clock read trails the wheel files a tick the
    /// wheel has passed in `pending`, which fires on the next pass.
    #[test]
    fn a_tick_the_wheel_has_passed_is_filed_pending() {
        let home = wheel_lock(100);
        let mut wheel = home.lock();
        let at_elapsed = pin!(SleepNode::new());
        let behind = pin!(SleepNode::new());
        register(home, &mut wheel, at_elapsed.as_ref(), 100);
        register(home, &mut wheel, behind.as_ref(), 50);

        assert_eq!(wheel.registered(), 2);
        assert_eq!(wheel.next_expiration_tick(), Some(100));
        assert!(wheel.fire_next(100).is_some());
        assert!(wheel.fire_next(100).is_some());
        assert!(wheel.fire_next(100).is_none());
        assert_eq!(at_elapsed.state(), NodeState::Done);
        assert_eq!(behind.state(), NodeState::Done);
    }
}
