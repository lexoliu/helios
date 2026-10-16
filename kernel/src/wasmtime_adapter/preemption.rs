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
//! - *Which store holds this processor?* The timer interrupt that ends
//!   the processor's guest time slice ([`crate::GUEST_TIME_SLICE`])
//!   preempts that store and no other. One slot per processor, indexed by
//!   [`current_processor`] and cache-padded: the processor itself writes
//!   its slot around every poll of a guest call ([`StoreInterrupts::drive`])
//!   and its own timer interrupt reads it ([`GuestInterrupts::preempt_current`]).
//!   No other processor reads or writes it, so a slot is never contended
//!   and the interrupt handler takes no lock.
//! - *Which stores belong to this instance?* A kill must reach every store
//!   of its victim, on whichever processor it runs or on none.
//!   The table is a lock-guarded slab written when a store is created and
//!   dropped and read by the kill path, all of it task context; no
//!   interrupt handler touches it, so it is a plain spin lock rather than
//!   an interrupt-masking one.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, Poll};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::{ProcessorId, current_processor};
use pin_project_lite::pin_project;
use slab::Slab;
use spin::Mutex;
use wasmtime::{Store, StoreInterruptHandle};

use crate::InstanceId;

/// The interrupt routes into every store of one engine.
pub struct GuestInterrupts {
    /// The store whose guest call this processor is polling, or null.
    ///
    /// Slot `n` is written only by processor `n`, around each poll of a
    /// guest call, and read only by processor `n`'s timer interrupt. The
    /// pointer names the handle inside the [`StoreInterrupts`] that the
    /// polled future borrows, so it is valid for exactly as long as it is
    /// published.
    running: Box<[CachePadded<AtomicPtr<StoreInterruptHandle>>]>,
    /// Every live store of this engine, by the instance it runs.
    stores: Mutex<Slab<KillTarget>>,
}

struct KillTarget {
    instance: InstanceId,
    handle: StoreInterruptHandle,
}

impl GuestInterrupts {
    /// Routes for an engine whose guests run on `processor_count`
    /// processors.
    pub(crate) fn new(processor_count: usize) -> Self {
        Self {
            running: (0..processor_count)
                .map(|_| CachePadded::new(AtomicPtr::new(ptr::null_mut())))
                .collect(),
            stores: Mutex::new(Slab::new()),
        }
    }

    /// Makes `store` reachable by kills against `instance` for as long as
    /// the returned registration lives, and lets its calls be preempted
    /// through [`StoreInterrupts::drive`].
    ///
    /// The store's engine must enable stack-limit interruption, which
    /// every kernel engine does (`build_component_engine_config`).
    pub(crate) fn register<T>(
        self: &Arc<Self>,
        instance: InstanceId,
        store: &Store<T>,
    ) -> StoreInterrupts {
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
            key,
        }
    }

    /// Preempts the guest running on this processor, if one is.
    ///
    /// Called from the processor's timer interrupt. The store's callback
    /// runs at the guest's next check and decides what the tick means
    /// for it: a yield to the executor, or nothing at all.
    pub fn preempt_current(&self) {
        let running = self.slot(current_processor()).load(Ordering::Acquire);
        // SAFETY: only this processor publishes into its slot, and it
        // clears the slot before the poll that published it returns, which
        // is before the `StoreInterrupts` the pointer names can be
        // dropped. This interrupt runs on the same processor, between two
        // instructions of that poll or outside any poll, so it observes
        // either a pointer that is still borrowed or null.
        if let Some(handle) = unsafe { running.as_ref() } {
            handle.interrupt();
        }
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

    fn slot(&self, processor: ProcessorId) -> &AtomicPtr<StoreInterruptHandle> {
        let index = usize::from(processor.id());
        self.running.get(index).unwrap_or_else(|| {
            panic!(
                "processor {index} runs guests but the engine's preemption table has {} slots",
                self.running.len()
            )
        })
    }
}

/// One store's interrupt route: registered for kills while it lives, and
/// published as its processor's running store while one of its calls is
/// polled.
pub(crate) struct StoreInterrupts {
    interrupts: Arc<GuestInterrupts>,
    handle: StoreInterruptHandle,
    key: usize,
}

impl StoreInterrupts {
    /// Drive `call`, a call into this store's guest, preemptible at the
    /// end of each time slice of whichever processor polls it.
    pub(crate) fn drive<Call: Future>(&self, call: Call) -> Preemptible<'_, Call> {
        Preemptible { call, store: self }
    }
}

impl Drop for StoreInterrupts {
    fn drop(&mut self) {
        self.interrupts.stores.lock().remove(self.key);
    }
}

pin_project! {
    /// A guest call whose store is this processor's running store while
    /// it is polled.
    pub(crate) struct Preemptible<'a, Call> {
        #[pin]
        call: Call,
        store: &'a StoreInterrupts,
    }
}

impl<Call: Future> Future for Preemptible<'_, Call> {
    type Output = Call::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Call::Output> {
        let this = self.project();
        let store = *this.store;
        let _running = RunningStore::publish(
            store.interrupts.slot(current_processor()),
            ptr::from_ref(&store.handle).cast_mut(),
        );
        this.call.poll(cx)
    }
}

/// This processor's running store for the length of one poll.
///
/// A poll runs on one processor from start to finish, and a turn of
/// another store inside it (a host call instantiating a child) publishes
/// and restores its own store around its own poll, so slots nest the way
/// the polls do: the previous store is restored when this one ends.
struct RunningStore<'a> {
    slot: &'a AtomicPtr<StoreInterruptHandle>,
    previous: *mut StoreInterruptHandle,
}

impl<'a> RunningStore<'a> {
    fn publish(
        slot: &'a AtomicPtr<StoreInterruptHandle>,
        handle: *mut StoreInterruptHandle,
    ) -> Self {
        // The reader is this processor's own interrupt handler, which sees
        // this processor's stores in program order; Release/Acquire keeps
        // the compiler from moving the handle's construction past the
        // publication.
        let previous = slot.swap(handle, Ordering::AcqRel);
        Self { slot, previous }
    }
}

impl Drop for RunningStore<'_> {
    fn drop(&mut self) {
        // Release: the restore cannot sink below the release that
        // eventually drops the published handle's owner, so the interrupt
        // handler never reads a pointer whose handle is gone.
        self.slot.store(self.previous, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::ptr;
    use core::sync::atomic::Ordering;
    use core::task::{Context, Poll, Waker};

    use helios_hal::cpu::current_processor;

    use super::{GuestInterrupts, StoreInterrupts};
    use crate::InstanceId;

    fn engine() -> wasmtime::Engine {
        let mut config = wasmtime::Config::new();
        config.stack_limit_interruption(true);
        wasmtime::Engine::new(&config).expect("an engine with stack-limit interruption builds")
    }

    fn running(interrupts: &GuestInterrupts) -> *const wasmtime::StoreInterruptHandle {
        interrupts.slot(current_processor()).load(Ordering::Acquire)
    }

    fn published(store: &StoreInterrupts) -> *const wasmtime::StoreInterruptHandle {
        ptr::from_ref(&store.handle)
    }

    fn poll_once<Call: Future>(call: core::pin::Pin<&mut Call>) -> Poll<Call::Output> {
        call.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// A store is this processor's running store exactly while one of its
    /// calls is polled, and a call into another store inside that poll
    /// hands the processor back to the outer store when it ends.
    #[test]
    fn a_poll_publishes_its_store_and_a_nested_poll_restores_the_outer_one() {
        let engine = engine();
        let interrupts = Arc::new(GuestInterrupts::new(1));
        let outer_store = wasmtime::Store::new(&engine, ());
        let inner_store = wasmtime::Store::new(&engine, ());
        let outer = interrupts.register(InstanceId::from_raw(1), &outer_store);
        let inner = interrupts.register(InstanceId::from_raw(2), &inner_store);

        assert!(running(&interrupts).is_null());
        let mut polls = 0;
        let call = outer.drive(poll_fn(|_| {
            polls += 1;
            assert_eq!(running(&interrupts), published(&outer));
            let mut nested = pin!(inner.drive(poll_fn(|_| {
                assert_eq!(running(&interrupts), published(&inner));
                Poll::Ready(())
            })));
            assert_eq!(poll_once(nested.as_mut()), Poll::Ready(()));
            assert_eq!(running(&interrupts), published(&outer));
            if polls == 1 {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
        let mut call = pin!(call);

        assert!(poll_once(call.as_mut()).is_pending());
        assert!(
            running(&interrupts).is_null(),
            "a suspended call is off the processor"
        );
        assert_eq!(poll_once(call.as_mut()), Poll::Ready(()));
        assert!(running(&interrupts).is_null());
    }

    /// A store leaves the kill table when its registration drops, and
    /// only the stores of the named instance are kill targets.
    #[test]
    fn the_kill_table_holds_exactly_the_live_registrations() {
        let engine = engine();
        let interrupts = Arc::new(GuestInterrupts::new(1));
        let first_store = wasmtime::Store::new(&engine, ());
        let second_store = wasmtime::Store::new(&engine, ());
        let first = interrupts.register(InstanceId::from_raw(7), &first_store);
        let second = interrupts.register(InstanceId::from_raw(7), &second_store);
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
