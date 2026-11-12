//! The compressed store over a real user pool: allocation, the frees other
//! processors make, the spare frame and the limits.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::MutexGuard;

use helios_hal::pmm::PhysFrame;

use super::slot::{
    Claim, CompressedLen, GRANULES_PER_FRAME, Gate, SlotAllocator, SlotTable, StoreFrameIndex,
    StoredObject, object_granules,
};
use super::store::{Limits, Store, StoreFrames, StoreFull, first_free_run, largest_free_run};
use super::test_pool::{POOL_FRAMES, allocated_bytes, serialise};

const PAGE: usize = PhysFrame::SIZE;

/// Real bytes for object payloads: source text, which is also what a page
/// of it would compress from.
const TEXT: &[u8] = include_bytes!("store.rs");

/// No limit worth the name: the whole pool.
const UNLIMITED: Limits = Limits {
    pool_frames: POOL_FRAMES,
    limit_percent: 100,
    accept_percent: 100,
};

/// The slots the objects belong to, and the frames and allocator under
/// test, with the pool held for the test's whole body.
struct Fixture {
    _turn: MutexGuard<'static, ()>,
    frames: StoreFrames,
    store: Store,
    slots: SlotTable,
    slot_allocator: SlotAllocator,
    /// The pool's allocation before the fixture took anything.
    baseline: usize,
}

/// One object put in the store, with the claim of its slot and the bytes it
/// should read back as.
struct Put {
    object: StoredObject,
    claim: Claim,
    block: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let turn = serialise();
        Self {
            _turn: turn,
            frames: StoreFrames::new(POOL_FRAMES),
            store: Store::new(),
            slots: SlotTable::new(),
            slot_allocator: SlotAllocator::new(),
            baseline: allocated_bytes(),
        }
    }

    /// An LZ4 block length that takes exactly `granules` granules.
    fn len(granules: usize) -> CompressedLen {
        let len = Gate::new(63)
            .expect("a gate")
            .accept(granules * 64 - 4)
            .expect("an object of fewer than 64 granules");
        assert_eq!(object_granules(len.get()), granules);
        len
    }

    /// Bytes of source text for a block of `granules` granules, from an
    /// offset that differs per `seed`.
    fn block(granules: usize, seed: usize) -> Vec<u8> {
        let len = Self::len(granules).get();
        let start = (seed * 97) % (TEXT.len() - len);
        TEXT[start..start + len].to_vec()
    }

    fn try_put(&mut self, granules: usize, seed: usize, limits: Limits) -> Result<Put, StoreFull> {
        self.store
            .ensure_spare(&self.frames)
            .expect("the pool has a frame");
        let claim = self
            .slot_allocator
            .reserve(&self.slots)
            .expect("a slot for the object");
        let block = Self::block(granules, seed);
        let object = self.store.allocate(
            &self.frames,
            claim.index(),
            &block,
            Self::len(granules),
            limits,
        )?;
        Ok(Put {
            object,
            claim,
            block,
        })
    }

    fn put(&mut self, granules: usize, seed: usize) -> Put {
        self.try_put(granules, seed, UNLIMITED)
            .expect("an unlimited store takes the object")
    }

    fn check_payload(&self, put: &Put) {
        assert_eq!(
            self.frames.payload(put.object, &put.claim),
            &put.block[..],
            "the object reads back what was stored"
        );
    }

    fn free(&mut self, put: &Put) {
        self.frames.free_object(put.object);
    }

    /// The bitmap of the granules an object takes.
    fn mask(put: &Put) -> u64 {
        let granules = object_granules(put.object.len().get());
        let first = put.object.first().raw();
        assert!(
            first + granules <= GRANULES_PER_FRAME,
            "an object stays inside its frame"
        );
        if granules == GRANULES_PER_FRAME {
            u64::MAX
        } else {
            ((1_u64 << granules) - 1) << first
        }
    }

    /// Returns every frame the fixture still holds to the pool and checks it
    /// all came back.
    fn finish(mut self, puts: &[Put]) {
        for put in puts {
            self.free(put);
        }
        self.store.drain(&self.frames);
        assert_eq!(self.store.object_frames(), 0, "every object frame is back");
        let spare = usize::from(self.store.has_spare()) * PAGE;
        assert_eq!(
            allocated_bytes(),
            self.baseline + spare,
            "only the spare is still held from the pool"
        );
    }
}

#[test]
fn free_runs_are_found_in_a_bitmap() {
    assert_eq!(largest_free_run(0), 64);
    assert_eq!(largest_free_run(u64::MAX), 0);
    // Granules 0, 1 and 3 in use: the free granule 2 is a run of one, and
    // 4..64 a run of sixty.
    assert_eq!(largest_free_run(0b1011), 60);
    assert_eq!(first_free_run(0, 64), Some(0));
    assert_eq!(first_free_run(0b1011, 1), Some(2));
    assert_eq!(first_free_run(0b1011, 2), Some(4));
    assert_eq!(first_free_run(u64::MAX, 1), None);
    assert_eq!(first_free_run(!0_u64 << 63, 63), Some(0));
    assert_eq!(first_free_run(1, 64), None);
}

/// Test 8: the best-fitting frame is the one whose largest free run is the
/// smallest that still fits, not the first or the emptiest.
#[test]
fn best_fit_picks_the_smallest_sufficient_bucket() {
    let mut fixture = Fixture::new();
    // Three frames, each too full for the next object, with free runs of
    // 14, 34 and 24 granules.
    let a = fixture.put(50, 1);
    let b = fixture.put(30, 2);
    let c = fixture.put(40, 3);
    let frames = [a.object.frame(), b.object.frame(), c.object.frame()];
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.raw())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "each object needed a frame of its own"
    );
    assert_eq!(fixture.store.object_frames(), 3);

    // 20 granules fit in b (34) and c (24); c is the tighter.
    let twenty = fixture.put(20, 4);
    assert_eq!(twenty.object.frame(), c.object.frame());
    assert_eq!(twenty.object.first().raw(), 40, "the lowest free run of c");

    // 10 granules fit in a (14) and b (34), and in neither c (4) nor a new
    // frame; a is the tighter.
    let ten = fixture.put(10, 5);
    assert_eq!(ten.object.frame(), a.object.frame());
    assert_eq!(ten.object.first().raw(), 50);
    assert_eq!(fixture.store.object_frames(), 3, "no new frame was needed");

    let puts = [a, b, c, twenty, ten];
    for put in &puts {
        fixture.check_payload(put);
    }
    fixture.finish(&puts);
}

/// Test 9: an object that fits no frame's largest run takes a new frame,
/// however much free space the frames have between them.
#[test]
fn no_object_spans_frames() {
    let mut fixture = Fixture::new();
    // Two frames with twenty free granules each: forty between them.
    let first = fixture.put(44, 1);
    let second = fixture.put(44, 2);
    assert_ne!(first.object.frame(), second.object.frame());

    let thirty = fixture.put(30, 3);
    assert_ne!(thirty.object.frame(), first.object.frame());
    assert_ne!(thirty.object.frame(), second.object.frame());
    assert_eq!(fixture.store.object_frames(), 3);

    // A long run of mixed sizes: every object lies inside one frame, no two
    // overlap, and each frame's bitmap is exactly its objects'.
    let mut puts = vec![first, second, thirty];
    let mut seed = 7_usize;
    for round in 0..300 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let granules = 1 + (seed >> 33) % 45;
        puts.push(fixture.put(granules, round));
    }
    let mut frames: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    for put in &puts {
        fixture.check_payload(put);
        let mask = Fixture::mask(put);
        let occupied = frames.entry(put.object.frame().raw()).or_default();
        assert_eq!(*occupied & mask, 0, "no two objects overlap");
        *occupied |= mask;
    }
    for (frame, expected) in frames {
        let frame = StoreFrameIndex::new(frame).unwrap();
        assert_eq!(
            fixture.frames.occupied(frame),
            expected,
            "a frame's bitmap is exactly its objects'"
        );
    }
    fixture.finish(&puts);
}

/// Test 10: another processor freeing objects of a frame while the owner
/// allocates in it leaves the occupancy exact.
#[test]
fn frees_from_another_thread_keep_occupancy_exact() {
    let mut fixture = Fixture::new();
    for round in 0..200 {
        // One frame of single-granule objects, then the owner allocates more
        // while another thread frees them all.
        let old: Vec<Put> = (0..32).map(|index| fixture.put(1, round + index)).collect();
        let shared_frame = old[0].object.frame();
        assert!(old.iter().all(|put| put.object.frame() == shared_frame));

        let released = AtomicBool::new(false);
        let mut fresh = Vec::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for put in &old {
                    fixture.frames.free_object(put.object);
                }
                released.store(true, Ordering::Release);
            });
            // The owner keeps allocating and draining until the frees are
            // done, then once more to settle.
            let mut seed = round;
            loop {
                let done = released.load(Ordering::Acquire);
                fixture.store.drain(&fixture.frames);
                for _ in 0..6 {
                    seed += 1;
                    fixture
                        .store
                        .ensure_spare(&fixture.frames)
                        .expect("the pool has a frame");
                    let claim = fixture
                        .slot_allocator
                        .reserve(&fixture.slots)
                        .expect("a slot");
                    let block = Fixture::block(1, seed);
                    let object = fixture
                        .store
                        .allocate(
                            &fixture.frames,
                            claim.index(),
                            &block,
                            Fixture::len(1),
                            UNLIMITED,
                        )
                        .expect("unlimited");
                    fresh.push(Put {
                        object,
                        claim,
                        block,
                    });
                }
                if done {
                    break;
                }
            }
        });
        fixture.store.drain(&fixture.frames);

        let mut expected: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
        for put in &fresh {
            fixture.check_payload(put);
            *expected.entry(put.object.frame().raw()).or_default() |= Fixture::mask(put);
        }
        for (frame, mask) in &expected {
            let frame = StoreFrameIndex::new(*frame).unwrap();
            assert_eq!(
                fixture.frames.occupied(frame),
                *mask,
                "occupancy is exactly the live objects' granules"
            );
        }
        for put in &fresh {
            fixture.free(put);
        }
        fixture.store.drain(&fixture.frames);
        assert_eq!(
            fixture.store.object_frames(),
            0,
            "round {round}: every frame emptied went back"
        );
    }
    fixture.finish(&[]);
}

/// Test 11: an empty frame goes back to the pool once, and the spare never
/// does.
#[test]
fn an_empty_frame_goes_back_to_the_pool_exactly_once_and_the_spare_never_does() {
    let mut fixture = Fixture::new();
    fixture.store.ensure_spare(&fixture.frames).unwrap();
    assert_eq!(allocated_bytes(), fixture.baseline + PAGE);
    assert!(fixture.store.has_spare());
    assert_eq!(fixture.store.frames(), 1);
    assert_eq!(fixture.store.object_frames(), 0, "the spare holds nothing");
    assert_eq!(fixture.store.drain(&fixture.frames), 0);
    assert_eq!(
        allocated_bytes(),
        fixture.baseline + PAGE,
        "the spare stays"
    );

    let put = fixture.put(10, 1);
    assert!(!fixture.store.has_spare(), "the object took the spare");
    fixture.store.ensure_spare(&fixture.frames).unwrap();
    assert_eq!(allocated_bytes(), fixture.baseline + 2 * PAGE);
    assert_eq!(fixture.store.frames(), 2);
    assert_eq!(fixture.store.object_frames(), 1);

    fixture.free(&put);
    assert_eq!(
        fixture.store.drain(&fixture.frames),
        1,
        "the emptied frame goes back"
    );
    assert_eq!(allocated_bytes(), fixture.baseline + PAGE);
    assert!(fixture.store.has_spare());
    assert_eq!(fixture.store.drain(&fixture.frames), 0, "and only once");
    assert_eq!(allocated_bytes(), fixture.baseline + PAGE);

    // A slow free that queued the frame after it went back: the entry
    // names a dead index and is ignored.
    let released = put.object.frame();
    fixture.frames.mark_dirty(released);
    assert_eq!(fixture.store.drain(&fixture.frames), 0);
    assert_eq!(allocated_bytes(), fixture.baseline + PAGE);

    // The same stale entry after the index is reused as the new spare: the
    // spare is not the store's to give back.
    let again = fixture.put(10, 2);
    fixture.store.ensure_spare(&fixture.frames).unwrap();
    assert_eq!(
        fixture.store.spare(),
        Some(released),
        "the released index is the spare's now"
    );
    fixture.frames.mark_dirty(released);
    let held = allocated_bytes();
    assert_eq!(fixture.store.drain(&fixture.frames), 0);
    assert!(
        fixture.store.has_spare(),
        "a stale entry cannot release the spare"
    );
    assert_eq!(allocated_bytes(), held);
    fixture.finish(&[again]);
}

#[test]
fn a_parked_page_owns_a_frame_and_goes_back_when_freed() {
    let mut fixture = Fixture::new();
    fixture.store.ensure_spare(&fixture.frames).unwrap();
    let claim = fixture.slot_allocator.reserve(&fixture.slots).unwrap();
    let page = &TEXT[..PAGE];

    let frame = fixture.store.park(&fixture.frames, &claim, page);
    assert!(!fixture.store.has_spare(), "parking consumed the spare");
    assert_eq!(fixture.store.object_frames(), 1);
    assert_eq!(fixture.frames.parked_page(frame, &claim), page);
    assert_eq!(fixture.frames.parked_slot(frame), claim.index().raw());
    assert_eq!(fixture.frames.occupied(frame), u64::MAX);
    assert_eq!(fixture.frames.starts(frame), 1);

    fixture.frames.free_parked(frame);
    assert_eq!(fixture.store.drain(&fixture.frames), 1);
    assert_eq!(fixture.store.object_frames(), 0);
    assert_eq!(allocated_bytes(), fixture.baseline);
}

/// Test 13: the limit refuses growth, and the refusal lasts until the store
/// is down to the accept level.
#[test]
fn the_limit_refuses_and_the_hysteresis_holds_it_until_the_accept_level() {
    let mut fixture = Fixture::new();
    // 20% of 100 frames, accepting again at 90% of that: 18.
    let limits = Limits {
        pool_frames: 100,
        limit_percent: 20,
        accept_percent: 90,
    };
    // Forty-granule objects: no two share a frame, so each takes one.
    let mut puts: Vec<Put> = (0..20)
        .map(|seed| fixture.try_put(40, seed, limits).expect("under the limit"))
        .collect();
    assert_eq!(fixture.store.object_frames(), 20);

    // At the limit a hole still takes an object that fits it...
    puts.push(
        fixture
            .try_put(10, 100, limits)
            .expect("a hole fill needs no frame"),
    );
    assert_eq!(fixture.store.object_frames(), 20);
    // ...but growth is refused, and the refusal begins.
    assert!(matches!(fixture.try_put(40, 101, limits), Err(StoreFull)));
    assert!(
        matches!(fixture.try_put(10, 102, limits), Err(StoreFull)),
        "a refusing store takes nothing, not even what fits a hole"
    );

    // One frame emptied: 19 frames, still above the accept level of 18.
    let freed = puts.remove(0);
    fixture.free(&freed);
    fixture.store.drain(&fixture.frames);
    assert_eq!(fixture.store.object_frames(), 19);
    assert!(matches!(fixture.try_put(10, 103, limits), Err(StoreFull)));

    // Two: 18 frames, at the accept level.
    let freed = puts.remove(0);
    fixture.free(&freed);
    fixture.store.drain(&fixture.frames);
    assert_eq!(fixture.store.object_frames(), 18);
    puts.push(fixture.try_put(40, 104, limits).expect("accepting again"));
    assert_eq!(fixture.store.object_frames(), 19);

    // A smaller pool lowers the limit at once: the caller reads it afresh.
    let shrunk = Limits {
        pool_frames: 50,
        ..limits
    };
    assert!(
        matches!(fixture.try_put(40, 105, shrunk), Err(StoreFull)),
        "19 frames is over a limit of 10"
    );

    let spare_free: Vec<Put> = puts;
    fixture.finish(&spare_free);
}
