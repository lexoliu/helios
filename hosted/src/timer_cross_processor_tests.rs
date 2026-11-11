//! Sleeps on one processor's timer, registered and dropped on another.
//!
//! The kernel's unit tests race the timer wheel from plain threads. This
//! runs it on two hosted processors over the real executor group:
//! processor 1 spawns global tasks that each race a sleep on processor
//! 0's timer against a signal. Half the signals are ready at once, so
//! those sleeps are registered by their first poll and dropped by
//! whichever processor ran the task; the other half fire on processor 0.
//! That exercises a remote registration, the wake it sends a parked
//! owner, a remote cancellation, and a real park and unpark.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::watchdog::ProgressCounter;
use helios_kernel::{Executor, Timer};

use crate::config::HostedConfig;
use crate::cpu::HostedCpu;
use crate::runtime::HostedMachine;

/// Three times the 4096 sleeps a timer held before #705.
const TASKS: usize = 3 * 4096;

/// Tasks spawned at a time. Each wave finishes before the next is
/// spawned, so the live tasks fit a floor-sized task arena (1 MiB, in
/// 256-byte blocks for a task this size).
const WAVE: usize = 1024;

/// How far out each sleep is set.
const SLEEP: Duration = Duration::from_millis(1);

const OWNER: ProcessorId = ProcessorId::new(0);
const SPAWNER: ProcessorId = ProcessorId::new(1);

/// The payload `kernel-prebuild` wrote for this host, named in the
/// `HELIOS_BOOTFS` path the Justfile exports for the test.
fn bootfs() -> PathBuf {
    PathBuf::from(
        std::env::var_os("HELIOS_BOOTFS")
            .expect("HELIOS_BOOTFS must name the kernel-prebuild payload"),
    )
}

/// Races a sleep on `owner_timer` against a signal that is ready at once
/// when `cancel` and never otherwise. The sleep is polled first, so it is
/// registered either way.
async fn race_sleep(owner_timer: Timer<HostedCpu>, cancel: bool, done: Arc<AtomicUsize>) {
    let signal = async move {
        if !cancel {
            core::future::pending::<()>().await;
        }
    };
    futures_lite::future::or(owner_timer.sleep_for(SLEEP), signal).await;
    done.fetch_add(1, Ordering::Release);
}

/// Processor 0: fires its timer and runs tasks until every task has
/// finished, parking when it has nothing to do. Returns how many sleeps
/// it fired.
fn owner_thread(
    machine: Arc<HostedMachine>,
    owner_timer: Arc<OnceLock<Timer<HostedCpu>>>,
    both_up: Arc<Barrier>,
    done: Arc<AtomicUsize>,
) -> JoinHandle<usize> {
    thread::Builder::new()
        .name("timer-owner-p0".to_owned())
        .spawn(move || {
            machine.register_thread(OWNER, thread::current());
            let cpu = HostedCpu::new(OWNER, machine.clone());
            let executor = Executor::new(ProgressCounter::new(), machine.processor_count(), OWNER);
            let timer = executor.timer(cpu.clone());
            if owner_timer.set(timer.clone()).is_err() {
                panic!("processor 0's timer was published twice");
            }
            both_up.wait();

            let mut fired = 0;
            while done.load(Ordering::Acquire) < TASKS {
                let fired_now = timer.fire_expired();
                fired += fired_now;
                let ran = executor
                    .run_until_stalled_with_stats(&timer)
                    .runnable_count();
                if fired_now == 0 && ran == 0 {
                    executor.park_until_work(&timer, || false);
                    // The hosted timer thread only unparks the processor;
                    // a backend's timer vector runs the handler that
                    // re-arms the next deadline, and this stands in for it.
                    timer.handle_interrupt();
                }
            }

            // Processor 1 may be waiting on the last task this one ran.
            cpu.wake_processor(SPAWNER);
            fired
        })
        .unwrap_or_else(|error| panic!("failed to spawn the timer owner's thread: {error}"))
}

/// Processor 1: spawns the tasks in waves and runs tasks until each wave
/// has finished. It never parks, so every wake the owner needs comes from
/// the timer's own handshake.
fn spawner_thread(
    machine: Arc<HostedMachine>,
    owner_timer: Arc<OnceLock<Timer<HostedCpu>>>,
    both_up: Arc<Barrier>,
    done: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("timer-spawner-p1".to_owned())
        .spawn(move || {
            machine.register_thread(SPAWNER, thread::current());
            let cpu = HostedCpu::new(SPAWNER, machine.clone());
            let executor =
                Executor::new(ProgressCounter::new(), machine.processor_count(), SPAWNER);
            let timer = executor.timer(cpu.clone());
            both_up.wait();
            let owner_timer = owner_timer
                .get()
                .expect("processor 0 publishes its timer before the barrier")
                .clone();
            let spawner = executor.spawner(cpu.clone());

            for wave_start in (0..TASKS).step_by(WAVE) {
                let wave_end = (wave_start + WAVE).min(TASKS);
                for task in wave_start..wave_end {
                    spawner.spawn_detached(race_sleep(
                        owner_timer.clone(),
                        task % 2 == 0,
                        done.clone(),
                    ));
                }
                while done.load(Ordering::Acquire) < wave_end {
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
        })
        .unwrap_or_else(|error| panic!("failed to spawn the spawning processor's thread: {error}"))
}

#[test]
fn sleeps_dropped_on_one_processor_leave_nothing_on_another_processors_timer() {
    let machine = HostedMachine::new(&HostedConfig::for_test(2, bootfs()));
    let owner_timer = Arc::new(OnceLock::new());
    let both_up = Arc::new(Barrier::new(2));
    let done = Arc::new(AtomicUsize::new(0));

    let owner = owner_thread(
        machine.clone(),
        owner_timer.clone(),
        both_up.clone(),
        done.clone(),
    );
    let spawner = spawner_thread(machine.clone(), owner_timer.clone(), both_up, done.clone());
    spawner.join().expect("the spawning processor panicked");
    let fired = owner.join().expect("the timer owner panicked");

    assert_eq!(done.load(Ordering::Acquire), TASKS);
    // Every sleep whose signal never came fired; a cancelled one fires
    // only if its task was held off its drop past the deadline.
    assert!(
        (TASKS / 2..=TASKS).contains(&fired),
        "{fired} of {TASKS} sleeps fired, but half of them can only end by firing"
    );
    let owner_timer = owner_timer.get().expect("processor 0 published its timer");
    assert_eq!(owner_timer.registered_sleeps(), 0);
}
