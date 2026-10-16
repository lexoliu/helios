//! How a running guest is interrupted: the end of its time slice, a
//! kill, a signal.
//!
//! Every store of an engine is interruptible through its
//! [`StoreInterruptHandle`]: a request lowers the store's published stack
//! limit to a sentinel, the guest's next function-entry or loop-header
//! check fails, and the store's interrupt callback runs in its place. A
//! request made while the guest is in host code or suspended on its fiber
//! is kept until the guest is next in wasm, so a requester never has to
//! know whether its target is running.
//!
//! [`GuestInterrupts`] is what the kernel holds to reach those handles. It
//! answers two questions, each with its own structure and its own
//! concurrency contract:
//!
//! - *Which store holds each processor, and since when?* Every
//!   processor's timer interrupt walks every processor's slot and
//!   interrupts each guest that has run for at least
//!   [`crate::GUEST_TIME_SLICE`] ([`GuestInterrupts::preempt_overdue`]),
//!   so no hardware timer has to run at the slice: the effective slice is
//!   the constant rounded up to the next timer interrupt of any
//!   processor, which [`crate::GUEST_TIME_SLICE`] quantifies per backend.
//!   The slots are a [`RunningSlots`], whose contract is below.
//! - *Which stores belong to this instance?* A kill must reach every store
//!   of its victim, on whichever processor it runs or on none.
//!   The table is a lock-guarded slab written when a store is created and
//!   dropped and read by the kill path, all of it task context; no
//!   interrupt handler touches it, so it is a plain spin lock rather than
//!   an interrupt-masking one.
//!
//! # The running slots
//!
//! One cache-padded block per processor, indexed by [`current_processor`].
//! Each block holds three words:
//!
//! - `target`, the handle of the store whose guest call the processor is
//!   polling, or null, and `started`, the processor's timebase when that
//!   run began. Only the owning processor writes them, around every poll
//!   of a guest call ([`StoreInterrupts::drive`]).
//! - `readers`, the number of timer interrupts, on any processor, that
//!   are between loading `target` and finishing with it. Every
//!   processor's timer interrupt increments and decrements it; the owner
//!   only reads it.
//!
//! Every operation is lock-free on the reader side and allocation-free on
//! both. A reader is a timer interrupt, which runs with the local
//! processor's interrupts masked and never waits for anything.
//!
//! Reclamation: a handle the owner publishes is valid until the poll
//! that published it returns, and nothing past that. A reader on another
//! processor may have loaded the pointer just before the owner withdrew
//! it, so withdrawing is a store of the replacement followed by a wait
//! until `readers` is zero. The store and the reader's increment are
//! sequentially consistent, as are the owner's load of `readers` and the
//! reader's load of `target`, so in their single total order either the
//! reader's load comes after the withdrawal, and it never sees the old
//! pointer, or its increment comes before the owner's load of `readers`,
//! and the owner waits for its decrement (a release the owner's load
//! acquires, so the reader's use of the handle happens before the poll
//! returns). The wait is bounded by one reader's critical section, a few
//! loads and the two atomic stores of [`StoreInterruptHandle::interrupt`];
//! under a hypervisor it also absorbs that reader's vCPU being descheduled
//! inside it. A reader that is the owner's own interrupt has finished
//! before the owner resumes, so the owner never waits for itself.
//!
//! The walk reads `started` only to decide, and the decision can be
//! wrong in one direction only: publishing writes `started` before
//! `target`, and withdrawing writes `target` before restoring `started`,
//! so a reader that rechecks `target` after reading `started` either
//! pairs a store with its own start or with a later one, and a later
//! start only defers the interrupt to the next tick.
//!
//! An interrupt can still land on a store whose poll ends between the
//! reader's decision and its request: the request then stays pending and
//! the guest yields at its first check after its next resumption. That
//! store had run a full slice, so the yield is the one it owed; the
//! window is the few instructions between the recheck and the request,
//! and closing it would need a way to retract a request, which the
//! store's handle does not offer.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::future::Future;
use core::hint::spin_loop;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::{Cpu, Instant, ProcessorId, Timebase, current_processor};
use pin_project_lite::pin_project;
use slab::Slab;
use spin::Mutex;
use wasmtime::{Store, StoreInterruptHandle};

use crate::{GUEST_TIME_SLICE, InstanceId};

/// The interrupt routes into every store of one engine.
pub struct GuestInterrupts {
    /// The store each processor is running and when its run began.
    running: RunningSlots<StoreInterruptHandle>,
    /// Every live store of this engine, by the instance it runs.
    stores: Mutex<Slab<KillTarget>>,
}

struct KillTarget {
    instance: InstanceId,
    handle: StoreInterruptHandle,
}

impl GuestInterrupts {
    /// Routes for an engine whose guests run on `processor_count`
    /// processors, whose clocks count in `timebase`.
    pub(crate) fn new(processor_count: usize, timebase: Timebase) -> Self {
        let slice_ticks = timebase.duration_to_ticks(GUEST_TIME_SLICE);
        assert!(
            slice_ticks != 0,
            "guest time slice {GUEST_TIME_SLICE:?} converted to zero timer ticks"
        );
        Self {
            running: RunningSlots::new(processor_count, slice_ticks),
            stores: Mutex::new(Slab::new()),
        }
    }

    /// Makes `store` reachable by kills against `instance` for as long as
    /// the returned registration lives, and lets its calls be preempted
    /// through [`StoreInterrupts::drive`], timed by `clock`.
    ///
    /// The store's engine must enable stack-limit interruption, which
    /// every kernel engine does (`build_component_engine_config`).
    pub(crate) fn register<T, Clock: Cpu>(
        self: &Arc<Self>,
        instance: InstanceId,
        store: &Store<T>,
        clock: Clock,
    ) -> StoreInterrupts<Clock> {
        let handle = store.interrupt_handle().unwrap_or_else(|error| {
            panic!("a kernel engine was built without stack-limit interruption: {error}")
        });
        let key = self.stores.lock().insert(KillTarget {
            instance,
            handle: handle.clone(),
        });
        StoreInterrupts {
            interrupts: self.clone(),
            handle,
            clock,
            key,
        }
    }

    /// Preempts every guest, on any processor, that has run for at least
    /// one [`crate::GUEST_TIME_SLICE`] by `now`.
    ///
    /// Called from every processor's timer interrupt. It takes no lock,
    /// allocates nothing, and sends no IPI: the request is an atomic
    /// store into the target store's context, which the guest takes at
    /// its next check. The store's callback decides what the request
    /// means for it: a yield to the executor, or nothing at all.
    pub fn preempt_overdue(&self, now: Instant) {
        let _ = (&self.running, now);
    }

    /// Interrupts every live store of `instance`, wherever it is: running
    /// on a processor, suspended on its fiber, or in host code.
    ///
    /// The kill path calls this after the instance's kill flag is set, so
    /// the callback each store runs at its next check finds the
    /// condemnation.
    pub(crate) fn interrupt_instance(&self, instance: InstanceId) {
        for (_, target) in self.stores.lock().iter() {
            if target.instance == instance {
                target.handle.interrupt();
            }
        }
    }
}

/// One store's interrupt route: registered for kills while it lives, and
/// published as its processor's running store while one of its calls is
/// polled.
pub(crate) struct StoreInterrupts<Clock> {
    interrupts: Arc<GuestInterrupts>,
    handle: StoreInterruptHandle,
    /// Stamps the start of each run, in the timebase every processor's
    /// timer interrupt reads.
    clock: Clock,
    key: usize,
}

impl<Clock: Cpu> StoreInterrupts<Clock> {
    /// Drive `call`, a call into this store's guest, preempted by any
    /// processor's timer interrupt once one poll of it has run for a
    /// guest time slice.
    pub(crate) fn drive<Call: Future>(&self, call: Call) -> Preemptible<'_, Clock, Call> {
        Preemptible { call, store: self }
    }
}

impl<Clock> Drop for StoreInterrupts<Clock> {
    fn drop(&mut self) {
        self.interrupts.stores.lock().remove(self.key);
    }
}

pin_project! {
    /// A guest call whose store is this processor's running store while
    /// it is polled.
    pub(crate) struct Preemptible<'a, Clock, Call> {
        #[pin]
        call: Call,
        store: &'a StoreInterrupts<Clock>,
    }
}

impl<Clock: Cpu, Call: Future> Future for Preemptible<'_, Clock, Call> {
    type Output = Call::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Call::Output> {
        let this = self.project();
        let store = *this.store;
        let _running =
            store
                .interrupts
                .running
                .publish(current_processor(), &store.handle, store.clock.now());
        this.call.poll(cx)
    }
}

/// A request to interrupt whatever a running slot names.
///
/// The kernel's only target is a store's [`StoreInterruptHandle`]; the
/// tests name their own, which record what reached them.
trait Interruptible {
    fn interrupt(&self);
}

impl Interruptible for StoreInterruptHandle {
    fn interrupt(&self) {
        StoreInterruptHandle::interrupt(self);
    }
}

/// One slot per processor naming the target it runs and when that run
/// began. The module docs state the concurrency contract.
struct RunningSlots<Target> {
    slots: Box<[CachePadded<RunningSlot<Target>>]>,
    /// [`crate::GUEST_TIME_SLICE`] in clock ticks.
    slice_ticks: u64,
}

/// One processor's block: `target` and `started` are written by that
/// processor only, `readers` by every processor's timer interrupt.
struct RunningSlot<Target> {
    target: AtomicPtr<Target>,
    started: AtomicU64,
    readers: AtomicUsize,
}

impl<Target: Interruptible> RunningSlots<Target> {
    fn new(processor_count: usize, slice_ticks: u64) -> Self {
        Self {
            slots: (0..processor_count)
                .map(|_| {
                    CachePadded::new(RunningSlot {
                        target: AtomicPtr::new(ptr::null_mut()),
                        started: AtomicU64::new(0),
                        readers: AtomicUsize::new(0),
                    })
                })
                .collect(),
            slice_ticks,
        }
    }

    /// Makes `target` `processor`'s running target, its run begun at
    /// `now`, until the returned guard drops.
    ///
    /// Only `processor` itself calls this, and `target` must stay valid
    /// until the guard has dropped. A poll runs on one processor from
    /// start to finish, and a turn of another store inside it (a host
    /// call instantiating a child) publishes and restores its own target
    /// around its own poll, so publications nest the way the polls do:
    /// the outer target and its start come back when the inner one ends.
    fn publish<'a>(
        &'a self,
        processor: ProcessorId,
        target: &'a Target,
        now: Instant,
    ) -> Published<'a, Target> {
        let slot = self.slot(processor);
        // Start first: a reader that sees the new target sees its start.
        // Only this processor writes either word, so its own load is
        // current.
        let started = slot.started.load(Ordering::Relaxed);
        slot.started.store(now.ticks(), Ordering::Release);
        let previous = slot
            .target
            .swap(ptr::from_ref(target).cast_mut(), Ordering::SeqCst);
        Published {
            slot,
            previous,
            started,
        }
    }

    /// Interrupts every published target that has run for at least one
    /// slice by `now`.
    fn interrupt_overdue(&self, now: Instant) {
        for slot in &self.slots {
            // An idle processor costs one load; the counted path below
            // runs only for a slot that names a target.
            if slot.target.load(Ordering::Relaxed).is_null() {
                continue;
            }
            slot.readers.fetch_add(1, Ordering::SeqCst);
            let target = slot.target.load(Ordering::SeqCst);
            if !target.is_null() {
                let started = slot.started.load(Ordering::Acquire);
                let overdue = now.ticks().saturating_sub(started) >= self.slice_ticks;
                if overdue && slot.target.load(Ordering::Relaxed) == target {
                    // SAFETY: `readers` counts this walk, and the owner
                    // that loaded `target` into the slot does not return
                    // from the withdrawal of `target` until `readers` is
                    // zero (module docs, "Reclamation"), so the target is
                    // still borrowed by the poll that published it.
                    unsafe { &*target }.interrupt();
                }
            }
            slot.readers.fetch_sub(1, Ordering::Release);
        }
    }

    fn slot(&self, processor: ProcessorId) -> &RunningSlot<Target> {
        let index = usize::from(processor.id());
        self.slots.get(index).unwrap_or_else(|| {
            panic!(
                "processor {index} runs guests but the engine's preemption table has {} slots",
                self.slots.len()
            )
        })
    }
}

/// A processor's running target for the length of one poll.
struct Published<'a, Target> {
    slot: &'a RunningSlot<Target>,
    previous: *mut Target,
    started: u64,
}

impl<Target> Drop for Published<'_, Target> {
    fn drop(&mut self) {
        // Target first: a reader that sees the restored start has already
        // seen the restored target, so its recheck refuses the old one.
        self.slot.target.store(self.previous, Ordering::SeqCst);
        self.slot.started.store(self.started, Ordering::Release);
        // A reader on another processor may have loaded the withdrawn
        // target before the store above; it is done with it once
        // `readers` drops to zero. The wait is another processor's few
        // instructions in an interrupt handler, the critical-section
        // contention `spin_loop` is for.
        while self.slot.readers.load(Ordering::SeqCst) != 0 {
            spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::ptr;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};

    use helios_hal::cpu::{Cpu, Instant, ProcessorId, Timebase, current_processor};

    use super::{GuestInterrupts, Interruptible, RunningSlots, StoreInterrupts};
    use crate::InstanceId;
    use crate::test_support::ManualClockCpu;

    fn engine() -> wasmtime::Engine {
        let mut config = wasmtime::Config::new();
        config.stack_limit_interruption(true);
        wasmtime::Engine::new(&config).expect("an engine with stack-limit interruption builds")
    }

    fn guest_interrupts(processor_count: usize) -> Arc<GuestInterrupts> {
        Arc::new(GuestInterrupts::new(
            processor_count,
            Timebase::new(1_000_000_000),
        ))
    }

    fn running(interrupts: &GuestInterrupts) -> (*const wasmtime::StoreInterruptHandle, u64) {
        let slot = interrupts.running.slot(current_processor());
        (
            slot.target.load(Ordering::Acquire),
            slot.started.load(Ordering::Acquire),
        )
    }

    fn published<Clock>(store: &StoreInterrupts<Clock>) -> *const wasmtime::StoreInterruptHandle {
        ptr::from_ref(&store.handle)
    }

    fn poll_once<Call: Future>(call: core::pin::Pin<&mut Call>) -> Poll<Call::Output> {
        call.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// A target that counts the interrupts that reach it, and counts as a
    /// violation any that reaches it after its owner withdrew it.
    struct Recorder<'a> {
        live: AtomicBool,
        interrupts: &'a AtomicUsize,
        violations: &'a AtomicUsize,
    }

    impl<'a> Recorder<'a> {
        fn new(interrupts: &'a AtomicUsize, violations: &'a AtomicUsize) -> Self {
            Self {
                live: AtomicBool::new(true),
                interrupts,
                violations,
            }
        }
    }

    impl Interruptible for Recorder<'_> {
        fn interrupt(&self) {
            if !self.live.load(Ordering::Relaxed) {
                self.violations.fetch_add(1, Ordering::Relaxed);
            }
            self.interrupts.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A store is this processor's running store, stamped with the
    /// clock at the start of the poll, exactly while one of its calls is
    /// polled, and a call into another store inside that poll hands the
    /// processor back to the outer store and its start when it ends.
    #[test]
    fn a_poll_publishes_its_store_and_a_nested_poll_restores_the_outer_one() {
        let engine = engine();
        let interrupts = guest_interrupts(1);
        let clock = ManualClockCpu::new();
        let outer_store = wasmtime::Store::new(&engine, ());
        let inner_store = wasmtime::Store::new(&engine, ());
        let outer = interrupts.register(InstanceId::from_raw(1), &outer_store, clock.clone());
        let inner = interrupts.register(InstanceId::from_raw(2), &inner_store, clock.clone());

        assert!(running(&interrupts).0.is_null());
        clock.advance(100);
        let mut polls = 0;
        let call = outer.drive(poll_fn(|_| {
            polls += 1;
            let outer_start = clock.now().ticks();
            assert_eq!(running(&interrupts), (published(&outer), outer_start));
            clock.advance(10);
            let mut nested = pin!(inner.drive(poll_fn(|_| {
                assert_eq!(running(&interrupts), (published(&inner), outer_start + 10));
                Poll::Ready(())
            })));
            assert_eq!(poll_once(nested.as_mut()), Poll::Ready(()));
            assert_eq!(running(&interrupts), (published(&outer), outer_start));
            if polls == 1 {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
        let mut call = pin!(call);

        assert!(poll_once(call.as_mut()).is_pending());
        assert!(
            running(&interrupts).0.is_null(),
            "a suspended call is off the processor"
        );
        clock.advance(1_000);
        assert_eq!(poll_once(call.as_mut()), Poll::Ready(()));
        assert!(running(&interrupts).0.is_null());
    }

    /// Any processor's tick interrupts every processor's target once it
    /// has run a slice, never one that has not, and never one whose run
    /// has ended; a nested run is timed from its own start and the outer
    /// run from its own once the nested one ends.
    #[test]
    fn a_tick_interrupts_every_processor_whose_run_has_lasted_a_slice() {
        let slots = RunningSlots::new(3, 100);
        let (count, violations) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let first = Recorder::new(&count, &violations);
        let third = Recorder::new(&count, &violations);
        let interrupted = |expected| assert_eq!(count.load(Ordering::Relaxed), expected);

        let first_run = slots.publish(ProcessorId::new(0), &first, Instant::new(0));
        let third_run = slots.publish(ProcessorId::new(2), &third, Instant::new(50));
        slots.interrupt_overdue(Instant::new(99));
        interrupted(0);
        slots.interrupt_overdue(Instant::new(100));
        interrupted(1);
        slots.interrupt_overdue(Instant::new(150));
        interrupted(3);

        drop(third_run);
        let nested = Recorder::new(&count, &violations);
        let nested_run = slots.publish(ProcessorId::new(0), &nested, Instant::new(160));
        slots.interrupt_overdue(Instant::new(200));
        interrupted(3);
        drop(nested_run);
        slots.interrupt_overdue(Instant::new(200));
        interrupted(4);
        drop(first_run);
        slots.interrupt_overdue(Instant::new(10_000));
        interrupted(4);
        assert_eq!(violations.load(Ordering::Relaxed), 0);
    }

    /// A target is never interrupted after its owner's withdrawal returns,
    /// however the ticks of other processors interleave with it.
    #[test]
    fn a_withdrawn_target_is_never_interrupted() {
        const RUNS: usize = 200_000;
        let slots = RunningSlots::new(2, 0);
        let (count, violations) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    while !done.load(Ordering::Relaxed) {
                        slots.interrupt_overdue(Instant::new(1));
                    }
                });
            }
            for run in 0..RUNS {
                let target = Recorder::new(&count, &violations);
                let processor = ProcessorId::new(u16::from(run % 2 == 1));
                let published = slots.publish(processor, &target, Instant::new(0));
                drop(published);
                target.live.store(false, Ordering::Relaxed);
            }
            done.store(true, Ordering::Relaxed);
        });
        assert_eq!(violations.load(Ordering::Relaxed), 0);
        assert!(
            count.load(Ordering::Relaxed) > 0,
            "the ticks never caught a published target, so the race went untested"
        );
    }

    /// A store leaves the kill table when its registration drops, and
    /// only the stores of the named instance are kill targets.
    #[test]
    fn the_kill_table_holds_exactly_the_live_registrations() {
        let engine = engine();
        let interrupts = guest_interrupts(1);
        let clock = ManualClockCpu::new();
        let first_store = wasmtime::Store::new(&engine, ());
        let second_store = wasmtime::Store::new(&engine, ());
        let first = interrupts.register(InstanceId::from_raw(7), &first_store, clock.clone());
        let second = interrupts.register(InstanceId::from_raw(7), &second_store, clock);
        let instances = |interrupts: &GuestInterrupts| {
            interrupts
                .stores
                .lock()
                .iter()
                .map(|(_, target)| target.instance)
                .collect::<alloc::vec::Vec<_>>()
        };
        assert_eq!(
            instances(&interrupts),
            [InstanceId::from_raw(7), InstanceId::from_raw(7)]
        );

        drop(first);
        assert_eq!(instances(&interrupts), [InstanceId::from_raw(7)]);
        drop(second);
        assert!(instances(&interrupts).is_empty());
    }
}
