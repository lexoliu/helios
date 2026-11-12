//! The freeze barrier on a hosted machine: four processors, each an OS
//! thread running the kernel's own run loop.
//!
//! The machine is booted once per test process (`helios_kernel::init`
//! keeps process-wide state) and shared by every test here, which take
//! turns. Each processor thread runs the loop `Kernel::run` runs, plus a
//! mailbox of jobs the test thread sends it, so a test can start a task
//! on a chosen processor without being one. Observations are collected by
//! the tasks and returned through channels: an assertion that failed on a
//! processor thread would take the machine down with it.

#![cfg(test)]

use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::Poll;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::{DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology};
use helios_kernel::{FreezeError, Kernel, Spawner, yield_now};

use crate::config::HostedConfig;
use crate::console::HostedConsole;
use crate::cpu::HostedCpu;
use crate::runtime::HostedMachine;

const PROCESSORS: usize = 4;

/// How long the test thread waits for anything a processor owes it. The
/// machine's own deadlines are milliseconds; this is the bound on a lost
/// wake-up, which would otherwise hang the test.
const PATIENCE: Duration = Duration::from_secs(20);

type Job = Box<dyn FnOnce(&'static Kernel<HostedCpu>) + Send>;

struct TestMachine {
    machine: Arc<HostedMachine>,
    mailboxes: Vec<Sender<Job>>,
    spawners: Vec<Spawner<HostedCpu>>,
    /// Timers each processor's run loop has fired, as its own run stats
    /// count them.
    timers_fired: Arc<[AtomicUsize]>,
    turn: Mutex<()>,
}

impl TestMachine {
    /// Runs `job` on processor `index`'s thread, between two passes of its
    /// run loop.
    fn on(&self, index: usize, job: impl FnOnce(&'static Kernel<HostedCpu>) + Send + 'static) {
        self.mailboxes[index]
            .send(Box::new(job))
            .expect("the processor thread is gone");
        self.machine
            .wake_processor(ProcessorId::new(u16::try_from(index).unwrap()));
    }

    /// Asks every processor but `skip` to answer a job, which proves its
    /// run loop is back.
    fn every_run_loop_answers(&self, skip: usize) {
        let (answer, answers) = unbounded();
        let mut expected = 0;
        for index in (0..PROCESSORS).filter(|index| *index != skip) {
            let answer = answer.clone();
            self.on(index, move |_| answer.send(index).unwrap());
            expected += 1;
        }
        for _ in 0..expected {
            answers
                .recv_timeout(PATIENCE)
                .expect("a processor's run loop did not come back from the freeze");
        }
    }
}

fn machine() -> (&'static TestMachine, MutexGuard<'static, ()>) {
    static MACHINE: OnceLock<TestMachine> = OnceLock::new();
    let machine = MACHINE.get_or_init(boot);
    // A test that failed poisons nothing the next one needs.
    let turn = machine
        .turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (machine, turn)
}

/// Starts the processor threads the way `HostedRuntime::run` does, but
/// runs a loop of this module's own after `init`, and returns once every
/// processor is in it.
fn boot() -> TestMachine {
    let bootfs = PathBuf::from(
        std::env::var_os("HELIOS_BOOTFS")
            .expect("HELIOS_BOOTFS must name the kernel-prebuild payload"),
    );
    let config = HostedConfig::for_test(PROCESSORS, bootfs);
    let machine = HostedMachine::new(&config);
    let timers_fired: Arc<[AtomicUsize]> = (0..PROCESSORS).map(|_| AtomicUsize::new(0)).collect();
    let (registered, registrations) = unbounded();
    let (online, onlines) = bounded(PROCESSORS);
    let mut mailboxes = Vec::new();

    for index in 0..PROCESSORS {
        let processor = ProcessorId::new(u16::try_from(index).unwrap());
        let (mailbox, jobs) = unbounded::<Job>();
        mailboxes.push(mailbox);
        let machine = machine.clone();
        let registered = registered.clone();
        let online = online.clone();
        let timers_fired = timers_fired.clone();
        thread::Builder::new()
            .name(format!("helios-freeze-test-p{index}"))
            .spawn(move || {
                machine.register_thread(processor, thread::current());
                registered.send(processor).unwrap();
                // The bootstrap processor wakes the others once the
                // kernel is up.
                thread::park();

                let cpu = HostedCpu::new(processor, machine.clone());
                let platform = Platform::new(
                    HostedConsole::new(),
                    machine.bootstrap_memory_regions(processor),
                    cpu.clone(),
                )
                .with_topology(
                    ProcessorTopology::start_all_secondaries(
                        machine.bootstrap_processor(),
                        machine.processor_count(),
                    )
                    .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
                )
                .with_dma_model(DmaModel::Translated)
                .with_devices(DeviceInventory::new());
                // The loop below never ends, so the kernel is never
                // dropped and a task may borrow it for good.
                let kernel: &'static Kernel<HostedCpu> =
                    Box::leak(Box::new(helios_kernel::init(platform)));
                cpu.install_timer(kernel.timer());
                if processor == machine.bootstrap_processor() {
                    for other in (0..machine.processor_count())
                        .map(|other| ProcessorId::new(u16::try_from(other).unwrap()))
                        .filter(|other| *other != processor)
                    {
                        cpu.start_processor(other);
                    }
                }
                online.send((index, kernel.spawner())).unwrap();

                loop {
                    while let Ok(job) = jobs.try_recv() {
                        job(kernel);
                    }
                    let stats = kernel.run_until_stalled_with_stats();
                    timers_fired[index].fetch_add(stats.timer_fired_count, Ordering::Relaxed);
                    if stats.progress_count() == 0 {
                        kernel.park_until_work();
                    }
                }
            })
            .expect("a processor thread starts");
    }

    drop((registered, online));
    for _ in 0..PROCESSORS {
        registrations
            .recv_timeout(PATIENCE)
            .expect("a processor thread did not register");
    }
    machine.wake_processor(machine.bootstrap_processor());
    let mut spawners = (0..PROCESSORS).map(|_| None).collect::<Vec<_>>();
    for _ in 0..PROCESSORS {
        let (index, spawner) = onlines
            .recv_timeout(PATIENCE)
            .expect("a processor did not come online");
        spawners[index] = Some(spawner);
    }
    TestMachine {
        machine,
        mailboxes,
        spawners: spawners.into_iter().map(Option::unwrap).collect(),
        timers_fired,
        turn: Mutex::new(()),
    }
}

#[track_caller]
fn recv<T>(receiver: &Receiver<T>) -> T {
    receiver
        .recv_timeout(PATIENCE)
        .expect("the freezer's task did not report")
}

/// What the freezer saw while it held the machine.
struct Frozen {
    queued_tasks_polled: usize,
    timers_fired: usize,
    sleepers_woken: usize,
}

/// A freeze with a task queued on every processor and a sleep due on
/// every processor's timer reaches the frozen state with no task polled
/// and no timer fired, and every queued task runs after the thaw.
#[test]
fn a_freeze_polls_no_task_and_fires_no_timer_and_a_thaw_runs_every_queued_task() {
    let (machine, _turn) = machine();
    let sleeper_polls = Arc::new(AtomicUsize::new(0));
    let (registered, registrations) = unbounded();
    // A sleep on each processor's timer, due 150 ms from now, that the
    // freeze has to straddle.
    for index in 0..PROCESSORS {
        let (registered, polls) = (registered.clone(), sleeper_polls.clone());
        machine.on(index, move |kernel| {
            let timer = kernel.timer();
            kernel.spawn_local_detached(async move {
                let mut sleep = pin!(timer.sleep_for(Duration::from_millis(150)));
                // Registers the sleep with the timer, then reports.
                std::future::poll_fn(|context| {
                    assert!(sleep.as_mut().poll(context).is_pending());
                    Poll::Ready(())
                })
                .await;
                registered.send(()).unwrap();
                sleep.await;
                polls.fetch_add(1, Ordering::SeqCst);
            });
        });
    }
    for _ in 0..PROCESSORS {
        recv(&registrations);
    }

    let queued_polls = Arc::new(AtomicUsize::new(0));
    let (queued_ran, queued_runs) = unbounded();
    let (report, reports) = bounded(1);
    let spawners = machine.spawners.clone();
    let timers_fired = machine.timers_fired.clone();
    let (sleepers, queued) = (sleeper_polls.clone(), queued_polls.clone());
    machine.on(0, move |kernel| {
        kernel.spawn_local_detached(async move {
            let frozen = kernel
                .freeze(|| {})
                .await
                .expect("a machine that yields freezes");
            let fired = || -> usize {
                timers_fired
                    .iter()
                    .map(|count| count.load(Ordering::Relaxed))
                    .sum()
            };
            let fired_before = fired();
            // A task queued on every processor's own queue, this one
            // included.
            for (index, spawner) in spawners.iter().enumerate() {
                let (queued, queued_ran) = (queued.clone(), queued_ran.clone());
                spawner.spawn_local_detached(async move {
                    queued.fetch_add(1, Ordering::SeqCst);
                    queued_ran.send(index).unwrap();
                });
            }
            // The sleeps are due well inside this window.
            thread::sleep(Duration::from_millis(250));
            let seen = Frozen {
                queued_tasks_polled: queued.load(Ordering::SeqCst),
                timers_fired: fired() - fired_before,
                sleepers_woken: sleepers.load(Ordering::SeqCst),
            };
            frozen.thaw();
            report.send(seen).unwrap();
        });
    });
    let seen = recv(&reports);
    assert_eq!(
        seen.queued_tasks_polled, 0,
        "a task was polled while frozen"
    );
    assert_eq!(seen.timers_fired, 0, "a timer fired while frozen");
    assert_eq!(seen.sleepers_woken, 0, "a sleeper ran while frozen");

    // After the thaw every queued task runs, on its own processor's
    // queue, and every overdue sleep fires.
    let mut ran = (0..PROCESSORS)
        .map(|_| recv(&queued_runs))
        .collect::<Vec<_>>();
    ran.sort_unstable();
    assert_eq!(ran, (0..PROCESSORS).collect::<Vec<_>>());
    let deadline = std::time::Instant::now() + PATIENCE;
    while sleeper_polls.load(Ordering::SeqCst) != PROCESSORS {
        assert!(
            std::time::Instant::now() < deadline,
            "an overdue sleep did not fire after the thaw"
        );
        thread::yield_now();
    }
}

/// A processor held in a task that never yields fails the freeze, by
/// name, and the machine is running afterwards: the processors that had
/// arrived are released, and a later freeze, once the task ends, works.
#[test]
fn a_task_that_never_yields_fails_the_freeze_naming_its_processor() {
    let (machine, _turn) = machine();
    let held = ProcessorId::new(2);
    let release = Arc::new(AtomicBool::new(false));
    let (started, starts) = bounded(1);
    let spinner_release = release.clone();
    machine.on(usize::from(held.id()), move |kernel| {
        kernel.spawn_local_detached(async move {
            started.send(()).unwrap();
            while !spinner_release.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
        });
    });
    recv(&starts);

    let (report, reports) = bounded(1);
    let outcome_release = release.clone();
    machine.on(0, move |kernel| {
        kernel.spawn_local_detached(async move {
            let outcome = kernel.freeze(|| {}).await.map(drop);
            report.send(outcome).unwrap();
            // The spinner ends, and the machine freezes again.
            outcome_release.store(true, Ordering::Release);
            let again = kernel.freeze(|| {}).await.map(drop);
            report.send(again).unwrap();
        });
    });
    assert_eq!(
        recv(&reports),
        Err(FreezeError::ProcessorDidNotArrive { processor: held })
    );
    assert_eq!(
        recv(&reports),
        Ok(()),
        "a machine whose freeze failed freezes once the task yields"
    );
    machine.every_run_loop_answers(0);
}

/// One freeze at a time: a second request while the machine is held is
/// refused without disturbing the first.
#[test]
fn a_second_freeze_is_refused_while_the_machine_is_held() {
    let (machine, _turn) = machine();
    let (report, reports) = bounded(1);
    machine.on(0, move |kernel| {
        kernel.spawn_local_detached(async move {
            let first = kernel.freeze(|| {}).await.expect("the first freeze holds");
            // Refused on its first poll: the task does not yield.
            let second = kernel.freeze(|| {}).await.map(drop);
            first.thaw();
            let third = kernel.freeze(|| {}).await.map(drop);
            report.send((second, third)).unwrap();
        });
    });
    let (second, third) = recv(&reports);
    assert_eq!(second, Err(FreezeError::AlreadyFreezing));
    assert_eq!(third, Ok(()), "a thawed machine freezes again");
    machine.every_run_loop_answers(0);
}

/// A thousand freezes and thaws while every processor runs a task that
/// yields in a loop: no freeze fails, no task is polled while one holds
/// the machine, and every processor is running again after each.
#[test]
fn a_thousand_freezes_under_load_lose_no_wake() {
    const CYCLES: usize = 1_000;
    let (machine, _turn) = machine();
    let stop = Arc::new(AtomicBool::new(false));
    let progress: Arc<[AtomicUsize]> = (0..PROCESSORS).map(|_| AtomicUsize::new(0)).collect();
    for index in 0..PROCESSORS {
        let (stop, progress) = (stop.clone(), progress.clone());
        machine.on(index, move |kernel| {
            kernel.spawn_local_detached(async move {
                while !stop.load(Ordering::Relaxed) {
                    progress[index].fetch_add(1, Ordering::Relaxed);
                    yield_now().await;
                }
            });
        });
    }
    let polls = |progress: &[AtomicUsize]| -> Vec<usize> {
        progress
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .collect()
    };

    let (report, reports) = bounded(1);
    let (loop_stop, loop_progress) = (stop.clone(), progress.clone());
    machine.on(0, move |kernel| {
        kernel.spawn_local_detached(async move {
            let started = polls(&loop_progress);
            let mut failure = None;
            for cycle in 0..CYCLES {
                let frozen = match kernel.freeze(|| {}).await {
                    Ok(frozen) => frozen,
                    Err(error) => {
                        failure = Some(format!("freeze {cycle} failed: {error}"));
                        break;
                    }
                };
                let before = polls(&loop_progress);
                thread::sleep(Duration::from_micros(100));
                let after = polls(&loop_progress);
                frozen.thaw();
                if before != after {
                    failure = Some(format!(
                        "freeze {cycle}: tasks ran while frozen, {before:?} then {after:?}"
                    ));
                    break;
                }
                // Lets this processor's own load task run, so the
                // machine is under load on every processor.
                yield_now().await;
            }
            let finished = polls(&loop_progress);
            loop_stop.store(true, Ordering::Relaxed);
            let advanced = started
                .iter()
                .zip(&finished)
                .all(|(started, finished)| finished > started);
            report.send((failure, advanced)).unwrap();
        });
    });
    let (failure, advanced) = recv(&reports);
    assert_eq!(failure, None);
    assert!(
        advanced,
        "a processor's task made no progress across the freezes"
    );
    machine.every_run_loop_answers(PROCESSORS);
}
