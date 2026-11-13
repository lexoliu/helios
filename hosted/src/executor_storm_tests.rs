//! A spawn storm drained by two processors.
//!
//! The kernel's unit tests cover the ready queues one thread at a time.
//! This runs two hosted processors on the real executor group: one spawns
//! more instance-funded tasks than any ready queue held before #615 and
//! both drain the global queue, which exercises the remote push, the
//! race between two consumers for the global queue's lock, and the idle
//! park and wake handshake.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};

use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::watchdog::ProgressCounter;
use helios_kernel::{Executor, InstanceRegistry, TaskFunding, Timer, UserMemoryPool};

use crate::config::DEFAULT_HEAP_BYTES;
use crate::cpu::HostedCpu;
use crate::runtime::HostedMachine;

/// Three times the capacity every ready queue had before #615 (4096), and
/// exactly what a floor-sized task arena admits of the smallest tasks:
/// 768 KiB of instance share in 64-byte blocks.
const TASKS: usize = 3 * 4096;

async fn counting_task(done: Arc<AtomicUsize>) {
    done.fetch_add(1, Ordering::Release);
}

/// Runs this processor's tasks until all of them have run, parking when
/// there is nothing to run.
fn drive(executor: &Executor, timer: &Timer<HostedCpu>, done: &AtomicUsize) {
    while done.load(Ordering::Acquire) < TASKS {
        if executor
            .run_until_stalled_with_stats(timer)
            .runnable_count()
            == 0
        {
            executor.park_until_work(timer, || false);
        }
    }
}

/// One hosted processor. The `storm` processor spawns every task once
/// both processors are up, then drains like the other one. `pool` is the
/// machine's one-node pool, which both processors' executors share.
fn processor_thread(
    machine: Arc<HostedMachine>,
    pool: &'static UserMemoryPool,
    processor: ProcessorId,
    both_up: Arc<Barrier>,
    done: Arc<AtomicUsize>,
    storm: bool,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("storm-p{}", processor.id()))
        .spawn(move || {
            machine.register_thread(processor, thread::current());
            let cpu = HostedCpu::new(processor, machine.clone());
            let executor = Executor::new(ProgressCounter::new(), pool, processor);
            let timer = executor.timer(cpu.clone());
            // A deadline the executor arms fires its interrupt step on
            // this thread, so the timer is installed as boot installs it.
            cpu.install_timer(timer.clone());
            both_up.wait();

            if storm {
                let registry = InstanceRegistry::new();
                let instance = registry.register("storm", 0);
                let spawner = executor
                    .spawner(cpu.clone())
                    .instance_spawner(TaskFunding::Instance, &instance);
                for spawned in 0..TASKS {
                    spawner
                        .try_spawn_detached(counting_task(done.clone()))
                        .unwrap_or_else(|refusal| {
                            panic!("the arena refused task {spawned} of {TASKS}: {refusal}")
                        });
                }
            }
            drive(&executor, &timer, &done);

            // The other processor may be parked on the last task this
            // one ran; a wake that lands before its park is kept.
            cpu.wake_processor(ProcessorId::new(1 - processor.id()));
        })
        .unwrap_or_else(|error| panic!("failed to spawn a storm processor thread: {error}"))
}

#[test]
fn two_processors_drain_a_global_spawn_storm_larger_than_the_old_queue() {
    let machine = HostedMachine::new(2, ProcessorId::new(0), DEFAULT_HEAP_BYTES);
    let both_up = Arc::new(Barrier::new(2));
    let done = Arc::new(AtomicUsize::new(0));
    let pool = crate::test_pool::single_node_pool(machine.processor_count());

    let processors = [
        processor_thread(
            machine.clone(),
            pool,
            ProcessorId::new(0),
            both_up.clone(),
            done.clone(),
            false,
        ),
        processor_thread(
            machine.clone(),
            pool,
            ProcessorId::new(1),
            both_up.clone(),
            done.clone(),
            true,
        ),
    ];
    for processor in processors {
        processor.join().expect("a storm processor panicked");
    }

    assert_eq!(done.load(Ordering::Acquire), TASKS);
}
