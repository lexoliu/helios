//! A hosted secondary processor taken offline and brought back by the
//! kernel, on the same machine and processor threads `hosted` runs.
//!
//! The test boots a two-processor machine the way `HostedRuntime` does —
//! `helios_kernel::init` on each processor's own thread, the bootstrap
//! processor starting the secondary — and drives the lifecycle from a
//! root future on the bootstrap processor.

#![cfg(test)]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::task::{Context, Waker};
use std::thread;
use std::time::Duration;

use futures_lite::future;
use helios_hal::cpu::{Cpu, ProcessorId, current_processor};
use helios_hal::memory::MemoryRegion;
use helios_hal::{DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology};
use helios_kernel::{Notify, OfflineError, OfflineRefusal, PinnedTasks, Sleep, Timer};

use crate::config::DEFAULT_HEAP_BYTES;
use crate::console::HostedConsole;
use crate::cpu::HostedCpu;
use crate::runtime::HostedMachine;

const BOOTSTRAP: ProcessorId = ProcessorId::new(0);
const SECONDARY: ProcessorId = ProcessorId::new(1);

/// How long any one step of the test may take before it is a hang.
const STEP_DEADLINE: Duration = Duration::from_secs(10);

fn platform(
    machine: &Arc<HostedMachine>,
    processor: ProcessorId,
    processors: usize,
) -> Platform<HostedConsole, HostedCpu, core::iter::Once<MemoryRegion>> {
    let cpu = HostedCpu::new(processor, machine.clone());
    Platform::new(
        HostedConsole::new(),
        machine.bootstrap_memory_regions(processor),
        cpu,
    )
    .with_topology(
        ProcessorTopology::start_all_secondaries(BOOTSTRAP, processors)
            .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
    )
    .with_dma_model(DmaModel::Translated)
    .with_devices(DeviceInventory::new())
}

/// Waits, a millisecond at a time, until `condition` holds; panics
/// naming `what` once [`STEP_DEADLINE`] has passed.
async fn eventually(timer: &Timer<HostedCpu>, what: &str, mut condition: impl FnMut() -> bool) {
    // The hosted timebase ticks once a nanosecond.
    let deadline = timer.now().saturating_add(
        u64::try_from(STEP_DEADLINE.as_nanos()).expect("the step deadline fits u64 nanoseconds"),
    );
    while !condition() {
        assert!(timer.now() < deadline, "timed out waiting for {what}");
        timer.sleep_for(Duration::from_millis(1)).await;
    }
}

#[test]
fn a_secondary_goes_offline_hands_its_timers_and_work_over_and_comes_back() {
    let machine = HostedMachine::new(2, BOOTSTRAP, DEFAULT_HEAP_BYTES);
    machine.register_thread(BOOTSTRAP, thread::current());

    let (registered_tx, registered_rx) = mpsc::channel();
    let (sleep_tx, sleep_rx) = mpsc::channel::<Sleep<HostedCpu>>();
    let release = Arc::new(Notify::new());
    {
        let machine = machine.clone();
        let release = release.clone();
        thread::Builder::new()
            .name("helios-lifecycle-p1".to_owned())
            .spawn(move || {
                machine.register_thread(SECONDARY, thread::current());
                registered_tx
                    .send(())
                    .expect("the test thread waits for registration");
                while !machine.is_started(SECONDARY) {
                    thread::park();
                }
                let kernel = helios_kernel::init(platform(&machine, SECONDARY, 2));
                HostedCpu::new(SECONDARY, machine.clone()).install_timer(kernel.timer());

                // A sleep first polled here is registered on this
                // processor's timer base; it is due long after the
                // processor has gone offline.
                let mut sleep = kernel.timer().sleep_for(Duration::from_millis(300));
                let mut context = Context::from_waker(Waker::noop());
                assert!(Pin::new(&mut sleep).poll(&mut context).is_pending());
                sleep_tx
                    .send(sleep)
                    .expect("the test thread takes the sleep");

                // Work pinned to this processor until the test lets it end.
                kernel.spawn_local_detached(async move { release.notified().await });
                kernel.run()
            })
            .expect("spawn the secondary processor's thread");
    }
    registered_rx
        .recv()
        .expect("the secondary registers its thread");

    let kernel = helios_kernel::init(platform(&machine, BOOTSTRAP, 2));
    let cpu = HostedCpu::new(BOOTSTRAP, machine.clone());
    cpu.install_timer(kernel.timer());
    cpu.start_processor(SECONDARY);
    let control = kernel.processor_control();
    let timer = kernel.timer();
    let spawner = kernel.spawner();

    kernel.run_local_future(async move {
        eventually(&timer, "the secondary to come online", || {
            control.online_set().contains(SECONDARY)
        })
        .await;
        let mut sleep = None;
        eventually(&timer, "the secondary's sleep", || {
            match sleep_rx.try_recv() {
                Ok(received) => {
                    sleep = Some(received);
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Disconnected) => panic!("the secondary dropped its sleep"),
            }
        })
        .await;
        let sleep = sleep.expect("the sleep arrived");

        // The pinned task cannot leave, so the secondary refuses and
        // stays online.
        assert_eq!(
            control.offline(SECONDARY).await,
            Err(OfflineError::Refused(OfflineRefusal::PinnedTasks(
                PinnedTasks {
                    processor: SECONDARY,
                    live: 1,
                }
            )))
        );
        assert!(control.online_set().contains(SECONDARY));
        assert_eq!(
            control.offline(BOOTSTRAP).await,
            Err(OfflineError::Bootstrap {
                processor: BOOTSTRAP
            })
        );

        // Once the pinned task has ended, the secondary goes.
        release.notify_one();
        let mut outcome = None;
        let mut attempts = 0;
        while outcome.is_none() {
            attempts += 1;
            assert!(attempts < 10_000, "the secondary never went offline");
            match control.offline(SECONDARY).await {
                Ok(()) => outcome = Some(()),
                Err(OfflineError::Refused(_)) => timer.sleep_for(Duration::from_millis(1)).await,
                Err(error) => panic!("offline failed: {error}"),
            }
        }
        assert!(!control.online_set().contains(SECONDARY));
        assert_eq!(control.online_set().count(), 1);
        assert_eq!(helios_kernel::online_processor_count(), 1);

        // Global work spawned while it is away runs on the processor
        // that is left.
        let tasks: Vec<_> = (0..64)
            .map(|_| spawner.spawn(async { current_processor() }))
            .collect();
        for task in tasks {
            assert_eq!(task.await, BOOTSTRAP);
        }

        // The sleep registered on the secondary's base fires although
        // the secondary is stopped: its entry moved to this processor.
        let fired = future::or(
            async {
                sleep.await;
                true
            },
            async {
                timer.sleep_for(STEP_DEADLINE).await;
                false
            },
        )
        .await;
        assert!(fired, "the stopped secondary's sleep never fired");

        // Back online, it is in the set and runs work again.
        control
            .online(SECONDARY)
            .await
            .expect("the secondary comes back");
        assert!(control.online_set().contains(SECONDARY));
        assert_eq!(control.online_set().count(), 2);
        let mut ran_on_secondary = false;
        for _ in 0..10_000 {
            let ran_on = spawner
                .spawn(async {
                    helios_kernel::yield_now().await;
                    current_processor()
                })
                .await;
            if ran_on == SECONDARY {
                ran_on_secondary = true;
                break;
            }
            timer.sleep_for(Duration::from_millis(1)).await;
        }
        assert!(
            ran_on_secondary,
            "no global task ran on the secondary after it came back"
        );
    });
}

/// A processor with nothing else running fires one sleep after another:
/// each expiry is the processor's timer interrupt, whose step arms the
/// next deadline.
#[test]
fn a_lone_processor_fires_consecutive_sleeps() {
    const SLEEPS: u32 = 5;
    let machine = HostedMachine::new(1, BOOTSTRAP, DEFAULT_HEAP_BYTES);
    let (done_tx, done_rx) = mpsc::channel();
    {
        let machine = machine.clone();
        thread::Builder::new()
            .name("helios-timer-p0".to_owned())
            .spawn(move || {
                machine.register_thread(BOOTSTRAP, thread::current());
                let kernel = helios_kernel::init(platform(&machine, BOOTSTRAP, 1));
                HostedCpu::new(BOOTSTRAP, machine.clone()).install_timer(kernel.timer());
                let timer = kernel.timer();
                kernel.run_local_future(async move {
                    for _ in 0..SLEEPS {
                        timer.sleep_for(Duration::from_millis(20)).await;
                    }
                });
                done_tx
                    .send(())
                    .expect("the test thread waits for the sleeps");
            })
            .expect("spawn the processor's thread");
    }
    // The wait is on a channel, not on the kernel's timer, so a timer that
    // never fires fails the test instead of hanging it.
    done_rx
        .recv_timeout(STEP_DEADLINE)
        .expect("the processor's consecutive sleeps never all fired");
}
