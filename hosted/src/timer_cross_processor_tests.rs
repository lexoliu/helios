//! Sleeps registered on one processor's timer base, dropped and awaited
//! on another.
//!
//! The kernel's unit tests race the timer wheel from plain threads. This
//! runs it on two hosted processors over the real executor group: a
//! sleep registers on the base of the processor that first polls it, so
//! processor 0 polls each sleep once, on its own base, and hands it to
//! processor 1, which spawns a global task per sleep that races it against
//! a signal. Half the signals are ready at once, so those sleeps are
//! dropped by whichever processor ran the task; the other half fire on
//! processor 0 and wake their task wherever it is queued, unless the
//! task's first poll comes after the deadline and completes the sleep
//! itself. That exercises
//! a remote cancellation, a waker registered from another processor, a
//! fire that wakes a task another processor runs, and a real park and
//! unpark of the base's owner.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Waker};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use futures_lite::future;
use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::watchdog::ProgressCounter;
use helios_kernel::{Executor, Sleep};

use crate::config::DEFAULT_HEAP_BYTES;
use crate::cpu::HostedCpu;
use crate::runtime::HostedMachine;

/// Three times the 4096 sleeps a timer held before #705.
const TASKS: usize = 3 * 4096;

/// Tasks spawned at a time. Each wave finishes before the next is
/// spawned, so the live tasks fit a floor-sized task arena (1 MiB).
const WAVE: usize = 1024;

/// How far out each sleep is set: long enough that a wave's tasks first
/// poll their sleeps before they are due, so most of the awaited half
/// can only end by firing.
const SLEEP: Duration = Duration::from_millis(10);

/// How long processor 1 waits for processor 0 to hand a sleep over
/// before the test is a hang.
const PATIENCE: Duration = Duration::from_secs(20);

const OWNER: ProcessorId = ProcessorId::new(0);
const SPAWNER: ProcessorId = ProcessorId::new(1);

/// A sleep processor 0 has registered and handed over. Boxed, because a
/// polled sleep is linked where it lies and cannot move.
type HandedSleep = Pin<Box<Sleep<HostedCpu>>>;

/// What the tasks report.
#[derive(Default)]
struct Outcome {
    /// Tasks finished.
    done: AtomicUsize,
    /// Awaited sleeps whose first poll in their task was pending. Nothing
    /// but a fire wakes that task again, so each of these ends by firing.
    must_fire: AtomicUsize,
}

/// Races `sleep` against a signal that is ready at once when `cancel` and
/// never otherwise.
async fn race_sleep(mut sleep: HandedSleep, cancel: bool, outcome: Arc<Outcome>) {
    let signal = async move {
        if !cancel {
            core::future::pending::<()>().await;
        }
    };
    let counted = outcome.clone();
    let mut first = true;
    // The closure owns the sleep, so the race below drops it before the
    // task reports done.
    let sleep = future::poll_fn(move |context| {
        let poll = sleep.as_mut().poll(context);
        if first && !cancel && poll.is_pending() {
            counted.must_fire.fetch_add(1, Ordering::Relaxed);
        }
        first = false;
        poll
    });
    future::or(sleep, signal).await;
    outcome.done.fetch_add(1, Ordering::Release);
}

/// Processor 0: registers the sleeps processor 1 asks for on its own
/// base, fires it and runs tasks until every task has finished, parking
/// when it has nothing to do. Returns how many sleeps it fired and how
/// many are still registered on its base.
fn owner_thread(
    machine: Arc<HostedMachine>,
    both_up: Arc<Barrier>,
    requests: Receiver<usize>,
    hand: Sender<HandedSleep>,
    outcome: Arc<Outcome>,
) -> JoinHandle<(usize, usize)> {
    thread::Builder::new()
        .name("timer-owner-p0".to_owned())
        .spawn(move || {
            machine.register_thread(OWNER, thread::current());
            let cpu = HostedCpu::new(OWNER, machine.clone());
            let executor = Executor::new(ProgressCounter::new(), machine.processor_count(), OWNER);
            let timer = executor.timer(cpu.clone());
            both_up.wait();

            let mut context = Context::from_waker(Waker::noop());
            let mut fired = 0;
            while outcome.done.load(Ordering::Acquire) < TASKS {
                while let Ok(count) = requests.try_recv() {
                    for _ in 0..count {
                        let mut sleep = Box::pin(timer.sleep_for(SLEEP));
                        assert!(sleep.as_mut().poll(&mut context).is_pending());
                        hand.send(sleep)
                            .expect("processor 1 takes every sleep it asked for");
                    }
                }
                let fired_now = timer.fire_expired();
                fired += fired_now;
                let ran = executor
                    .run_until_stalled_with_stats(&timer)
                    .runnable_count();
                if fired_now == 0 && ran == 0 {
                    executor.park_until_work(&timer, || !requests.is_empty());
                    // The hosted timer thread only unparks the processor;
                    // a backend's timer vector runs the handler that
                    // re-arms the next deadline, and this stands in for it.
                    timer.handle_interrupt();
                }
            }

            // Processor 1 may be waiting on the last task this one ran.
            cpu.wake_processor(SPAWNER);
            (fired, timer.registered_sleeps())
        })
        .unwrap_or_else(|error| panic!("failed to spawn the timer owner's thread: {error}"))
}

/// Processor 1: asks processor 0 for each wave's sleeps, spawns a task
/// per sleep, and runs tasks until the wave has finished. It never parks
/// and registers nothing on its own base.
fn spawner_thread(
    machine: Arc<HostedMachine>,
    both_up: Arc<Barrier>,
    request: Sender<usize>,
    handed: Receiver<HandedSleep>,
    outcome: Arc<Outcome>,
) -> JoinHandle<usize> {
    thread::Builder::new()
        .name("timer-spawner-p1".to_owned())
        .spawn(move || {
            machine.register_thread(SPAWNER, thread::current());
            let cpu = HostedCpu::new(SPAWNER, machine.clone());
            let executor =
                Executor::new(ProgressCounter::new(), machine.processor_count(), SPAWNER);
            let timer = executor.timer(cpu.clone());
            both_up.wait();
            let spawner = executor.spawner(cpu.clone());

            for wave_start in (0..TASKS).step_by(WAVE) {
                let wave_end = (wave_start + WAVE).min(TASKS);
                request
                    .send(wave_end - wave_start)
                    .expect("processor 0 takes every request");
                cpu.wake_processor(OWNER);
                for task in wave_start..wave_end {
                    let sleep = handed
                        .recv_timeout(PATIENCE)
                        .expect("processor 0 never handed a sleep over");
                    spawner.spawn_detached(race_sleep(sleep, task % 2 == 0, outcome.clone()));
                }
                while outcome.done.load(Ordering::Acquire) < wave_end {
                    if executor
                        .run_until_stalled_with_stats(&timer)
                        .runnable_count()
                        == 0
                    {
                        thread::yield_now();
                    }
                }
            }

            // Processor 0 may be parked after the last task finished here.
            cpu.wake_processor(OWNER);
            timer.registered_sleeps()
        })
        .unwrap_or_else(|error| panic!("failed to spawn the spawning processor's thread: {error}"))
}

#[test]
fn sleeps_dropped_on_one_processor_leave_nothing_on_another_processors_timer() {
    let machine = HostedMachine::new(2, OWNER, DEFAULT_HEAP_BYTES);
    let both_up = Arc::new(Barrier::new(2));
    let outcome = Arc::new(Outcome::default());
    let (request, requests) = unbounded();
    let (hand, handed) = unbounded();

    let owner = owner_thread(
        machine.clone(),
        both_up.clone(),
        requests,
        hand,
        outcome.clone(),
    );
    let spawner = spawner_thread(machine.clone(), both_up, request, handed, outcome.clone());
    let spawner_registered = spawner.join().expect("the spawning processor panicked");
    let (fired, owner_registered) = owner.join().expect("the timer owner panicked");

    assert_eq!(outcome.done.load(Ordering::Acquire), TASKS);
    // Every awaited sleep still pending at its task's first poll fired and
    // woke its task, wherever it ran; a cancelled one fires only if its
    // task was held off its drop past the deadline, and an awaited one
    // first polled after its deadline completes in that poll.
    let must_fire = outcome.must_fire.load(Ordering::Relaxed);
    assert!(must_fire > 0, "no task polled its sleep before it was due");
    assert!(
        (must_fire..=TASKS).contains(&fired),
        "{fired} of {TASKS} sleeps fired, but {must_fire} of them could only end by firing"
    );
    assert_eq!(owner_registered, 0);
    assert_eq!(spawner_registered, 0);
}
