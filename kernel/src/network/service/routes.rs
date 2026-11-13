//! Which processor drains each queue pair, and which pairs the device
//! has live.
//!
//! # The layout
//!
//! The service has one shard per *possible* processor, and the
//! ephemeral port range is split across those shards once, at
//! construction: neither ever re-partitions, so a socket's shard and
//! its ports outlive any processor going offline. What follows the
//! online set is the queue pairs.
//!
//! The device brought up `brought_up` pairs, never more than the
//! possible processors. A multiqueue device activates pairs by count,
//! so the live pairs are the prefix `0..active`, and `active` is the
//! smaller of `brought_up` and the online processor count: every online
//! processor drains at most one pair, and a pair is live only while an
//! online processor drains it. Each live pair has one owner — the
//! processor whose packet pump drains it and to which the device
//! delivers its interrupts. An online processor past the live count
//! owns no pair; its pump parks until a hand-off gives it one.
//!
//! A shard submits on, and is served by the pump of, its home pair
//! `shard % brought_up` while that pair is live, and otherwise on the
//! live pair the home pair folds onto, `home % active`. A steering
//! device's indirection table folds its slots by the same rule, so a
//! frame it steers still arrives on the pair whose pump serves the
//! frame's shard.
//!
//! At boot pair `i` is owned by processor `i`, which is where every
//! backend binds the pair's interrupt; a processor's first bring-up is
//! not a transition and changes nothing here.
//!
//! # Transitions
//!
//! A departing processor that owns a pair hands it over before it stops:
//!
//! - when an online processor owns no pair, that processor adopts the
//!   pair and the live count stays;
//! - otherwise the live count drops by one: the last live pair stops
//!   being live, and when it is not the departing processor's own its
//!   owner moves onto the departing processor's pair.
//!
//! An arriving processor takes the next pair when one is not live, and
//! the live count grows by one; otherwise it owns none.
//!
//! # Concurrency contract
//!
//! - Writers: only the network lifecycle step, on the transitioning
//!   processor, while the initiator holds the lifecycle's transition
//!   lock. At most one transition runs at a time and none during a
//!   machine freeze, so the table has one writer and every write is a
//!   plain `Release` store — nothing here takes a lock.
//! - Readers: any processor, lock-free, through `Acquire` loads: a pump
//!   reads its own pair once per round, and every submission folds its
//!   shard onto a pair with one load of `active`. A submission that
//!   loaded the old count just before a shrink can land one batch on the
//!   pair that is stopping; the device then holds it until the pair is
//!   live again or the stack retransmits it, which costs that batch's
//!   latency and never correctness.
//! - `changed` is raised after every published change. An idle pump arms
//!   it before it reads its own pair, so a hand-off that gives it one
//!   cannot be slept through.

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use helios_hal::cpu::ProcessorId;

use crate::exec::OnlineSet;
use crate::{ProgressMark, ProgressSignal};

/// A processor that drains no pair.
const NO_PAIR: u16 = u16::MAX;

/// The live-pair layout at one instant: what a caller folds shards
/// with when it needs one consistent answer across several shards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PairLayout {
    brought_up: usize,
    active: usize,
}

impl PairLayout {
    /// The live pair shard `shard` submits on and is served from.
    pub(super) const fn pair_of_shard(self, shard: usize) -> usize {
        (shard % self.brought_up) % self.active
    }

    /// How many pairs are live.
    pub(super) const fn active(self) -> usize {
        self.active
    }
}

/// What a departing processor's pair becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Departure {
    /// The processor drains no pair: nothing moves.
    Idle,
    /// `by`, an online processor that drained nothing, adopts `pair`;
    /// the live count stays.
    Adopted { pair: usize, by: ProcessorId },
    /// The live count drops to `active`, so pair `active` stops being
    /// live. When that pair is not the departing processor's own, its
    /// owner `by` moves onto the departing processor's `pair`.
    Shrunk {
        pair: usize,
        active: usize,
        by: Option<ProcessorId>,
    },
}

/// What an arriving processor takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arrival {
    /// Every brought-up pair is live already: it owns none.
    Idle,
    /// Pair `pair` becomes live with the processor as its owner, and the
    /// live count grows to `pair + 1`.
    Grown { pair: usize },
}

/// The queue-pair routing table. See the module docs for the layout
/// and the concurrency contract.
pub(super) struct PairRoutes {
    active: AtomicUsize,
    /// The processor each brought-up pair's interrupts are delivered to:
    /// its owner while it is live, and the owner of the pair it folds
    /// onto after it stopped being live.
    owners: Box<[AtomicU16]>,
    /// The live pair each possible processor drains, or [`NO_PAIR`].
    drains: Box<[AtomicU16]>,
    changed: ProgressSignal,
}

impl PairRoutes {
    /// The boot layout: every brought-up pair live, pair `i` owned by
    /// processor `i`.
    pub(super) fn new(brought_up: usize, possible_processors: usize) -> Self {
        assert!(brought_up != 0, "an interface has at least one queue pair");
        assert!(
            brought_up <= possible_processors,
            "the device was brought up with {brought_up} queue pairs on a \
             {possible_processors}-processor machine: a pair past the processor \
             count is activated on the device and drained by nobody"
        );
        let index = |value: usize| {
            u16::try_from(value).unwrap_or_else(|_| panic!("{value} does not fit a pair index"))
        };
        Self {
            active: AtomicUsize::new(brought_up),
            owners: (0..brought_up)
                .map(|pair| AtomicU16::new(index(pair)))
                .collect(),
            drains: (0..possible_processors)
                .map(|processor| {
                    AtomicU16::new(if processor < brought_up {
                        index(processor)
                    } else {
                        NO_PAIR
                    })
                })
                .collect(),
            changed: ProgressSignal::new(),
        }
    }

    /// How many pairs the device brought up.
    pub(super) fn brought_up(&self) -> usize {
        self.owners.len()
    }

    /// The layout as of this load.
    pub(super) fn layout(&self) -> PairLayout {
        PairLayout {
            brought_up: self.brought_up(),
            active: self.active.load(Ordering::Acquire),
        }
    }

    /// The live pair shard `shard` submits on and is served from.
    pub(super) fn pair_of_shard(&self, shard: usize) -> usize {
        self.layout().pair_of_shard(shard)
    }

    /// The live pair `processor` drains, or `None` when it drains none.
    pub(super) fn drained_by(&self, processor: ProcessorId) -> Option<usize> {
        let pair = self.drain_slot(processor).load(Ordering::Acquire);
        (pair != NO_PAIR).then_some(usize::from(pair))
    }

    /// The processor `pair`'s interrupts are delivered to.
    pub(super) fn owner(&self, pair: usize) -> ProcessorId {
        ProcessorId::new(self.owner_slot(pair).load(Ordering::Acquire))
    }

    /// Samples the change signal, before the caller reads its pair.
    pub(super) fn mark(&self) -> ProgressMark {
        self.changed.mark()
    }

    /// The change signal a sampled mark belongs to.
    pub(super) fn changed(&self) -> &ProgressSignal {
        &self.changed
    }

    /// What `departing`'s pair becomes, given the processors online now
    /// (`departing` among them). Reads only; the caller applies it.
    pub(super) fn plan_departure(&self, departing: ProcessorId, online: &OnlineSet) -> Departure {
        let Some(pair) = self.drained_by(departing) else {
            return Departure::Idle;
        };
        let idle = (0..self.drains.len()).map(processor_id).find(|&processor| {
            processor != departing
                && online.contains(processor)
                && self.drained_by(processor).is_none()
        });
        if let Some(by) = idle {
            return Departure::Adopted { pair, by };
        }
        let live = self.active.load(Ordering::Acquire);
        assert!(
            live > 1,
            "processor {} departs owning pair {pair}, the last live pair, with no online \
             processor to take it: the bootstrap processor stays online and drains a pair or \
             is idle",
            departing.id()
        );
        let last = live - 1;
        Departure::Shrunk {
            pair,
            active: last,
            by: (pair != last).then(|| self.owner(last)),
        }
    }

    /// What `arriving` takes. Reads only; the caller applies it.
    pub(super) fn plan_arrival(&self, arriving: ProcessorId) -> Arrival {
        assert!(
            self.drained_by(arriving).is_none(),
            "processor {} arrives already draining a pair",
            arriving.id()
        );
        let live = self.active.load(Ordering::Acquire);
        if live < self.brought_up() {
            Arrival::Grown { pair: live }
        } else {
            Arrival::Idle
        }
    }

    /// Publishes the live count. Shrinking is published before the
    /// device is told, so no new submission lands on the pair that is
    /// stopping; growing after, so none lands on a pair the device does
    /// not read yet.
    pub(super) fn publish_active(&self, active: usize) {
        assert!(
            (1..=self.brought_up()).contains(&active),
            "{active} live pairs out of {} brought up",
            self.brought_up()
        );
        self.active.store(active, Ordering::Release);
    }

    /// Records that `pair`'s interrupts go to `processor`.
    pub(super) fn publish_owner(&self, pair: usize, processor: ProcessorId) {
        self.owner_slot(pair)
            .store(processor.id(), Ordering::Release);
    }

    /// Records the live pair `processor` drains, or that it drains none.
    pub(super) fn publish_drain(&self, processor: ProcessorId, pair: Option<usize>) {
        let pair = pair.map_or(NO_PAIR, |pair| {
            u16::try_from(pair).unwrap_or_else(|_| panic!("pair {pair} does not fit a pair index"))
        });
        self.drain_slot(processor).store(pair, Ordering::Release);
    }

    /// Raises the change signal, after the stores of one transition.
    pub(super) fn announce(&self) {
        self.changed.signal();
    }

    fn drain_slot(&self, processor: ProcessorId) -> &AtomicU16 {
        self.drains
            .get(usize::from(processor.id()))
            .unwrap_or_else(|| {
                panic!(
                    "processor {} is outside the {} possible processors the routes cover",
                    processor.id(),
                    self.drains.len()
                )
            })
    }

    fn owner_slot(&self, pair: usize) -> &AtomicU16 {
        self.owners.get(pair).unwrap_or_else(|| {
            panic!(
                "pair {pair} is outside the {} pairs the device brought up",
                self.owners.len()
            )
        })
    }
}

fn processor_id(index: usize) -> ProcessorId {
    ProcessorId::new(
        u16::try_from(index)
            .unwrap_or_else(|_| panic!("processor {index} does not fit a processor identifier")),
    )
}

#[cfg(test)]
mod tests {
    use helios_hal::cpu::ProcessorId;

    use super::{Arrival, Departure, PairRoutes};
    use crate::exec::OnlineSet;

    const P: fn(u16) -> ProcessorId = ProcessorId::new;

    fn online(processors: &[u16], possible: usize) -> OnlineSet {
        let set = OnlineSet::new(possible);
        for processor in processors {
            set.insert(P(*processor));
        }
        set
    }

    /// Shards fold onto their home pair while it is live and onto a live
    /// pair otherwise; a live home pair keeps every shard it had.
    #[test]
    fn a_shard_keeps_its_pair_while_the_pair_is_live() {
        let routes = PairRoutes::new(4, 8);
        let full: alloc::vec::Vec<_> = (0..8).map(|shard| routes.pair_of_shard(shard)).collect();
        assert_eq!(full, [0, 1, 2, 3, 0, 1, 2, 3]);
        routes.publish_active(3);
        let shrunk: alloc::vec::Vec<_> = (0..8).map(|shard| routes.pair_of_shard(shard)).collect();
        assert_eq!(shrunk, [0, 1, 2, 0, 0, 1, 2, 0]);
    }

    /// With every online processor draining a pair, a departure shrinks
    /// the live count, and the last pair's owner moves onto the
    /// departing pair; a departure of the last pair's own owner moves
    /// nobody.
    #[test]
    fn a_departure_with_no_idle_processor_shrinks_the_live_pairs() {
        let routes = PairRoutes::new(4, 4);
        let set = online(&[0, 1, 2, 3], 4);
        assert_eq!(
            routes.plan_departure(P(1), &set),
            Departure::Shrunk {
                pair: 1,
                active: 3,
                by: Some(P(3)),
            }
        );
        assert_eq!(
            routes.plan_departure(P(3), &set),
            Departure::Shrunk {
                pair: 3,
                active: 3,
                by: None,
            }
        );
    }

    /// An online processor that drains nothing adopts the departing
    /// pair, and the live count stays.
    #[test]
    fn an_idle_online_processor_adopts_a_departing_pair() {
        let routes = PairRoutes::new(2, 4);
        let set = online(&[0, 1, 2, 3], 4);
        assert_eq!(
            routes.plan_departure(P(1), &set),
            Departure::Adopted { pair: 1, by: P(2) }
        );
        assert_eq!(routes.plan_departure(P(3), &set), Departure::Idle);
        // An idle processor that is itself offline adopts nothing.
        let set = online(&[0, 1, 3], 4);
        assert_eq!(
            routes.plan_departure(P(1), &set),
            Departure::Adopted { pair: 1, by: P(3) }
        );
    }

    /// An arrival takes the next pair while one is not live, and nothing
    /// once every pair is.
    #[test]
    fn an_arrival_takes_the_next_pair_that_is_not_live() {
        let routes = PairRoutes::new(4, 5);
        assert_eq!(routes.plan_arrival(P(4)), Arrival::Idle);
        routes.publish_drain(P(3), None);
        routes.publish_active(3);
        assert_eq!(routes.plan_arrival(P(3)), Arrival::Grown { pair: 3 });
    }

    #[test]
    #[should_panic(expected = "queue pairs on a 2-processor machine")]
    fn more_pairs_than_possible_processors_fails_fast() {
        let _ = PairRoutes::new(4, 2);
    }

    #[test]
    #[should_panic(expected = "the last live pair, with no online processor to take it")]
    fn the_last_live_pair_cannot_be_handed_to_nobody() {
        let routes = PairRoutes::new(1, 2);
        let set = online(&[0], 2);
        let _ = routes.plan_departure(P(0), &set);
    }
}
