//! The timer's interrupt step on a hosted machine with one processor.
//!
//! The processor is the only thread that can wake itself for a sleep, so
//! each sleep after the first fires only if the timer thread's expiry
//! ran `Timer::handle_interrupt` on the processor and moved the armed
//! deadline off the one that just passed.

#![cfg(test)]

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded};
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::SingleNode;
use helios_hal::{DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology};
use helios_kernel::Kernel;

use crate::config::HostedConfig;
use crate::console::HostedConsole;
use crate::cpu::HostedCpu;
use crate::runtime::HostedMachine;

/// How long the test waits for a sleep to fire. The sleeps below are tens
/// of milliseconds; this bounds a lost interrupt, which would otherwise
/// hang the test.
const PATIENCE: Duration = Duration::from_secs(20);

/// Each sleep's length. Shorter than the timer's idle interrupt interval
/// (100 ms), so a sleep after the first is earlier than the deadline the
/// interrupt step re-arms and nothing but that step can arm it.
const SLEEP: Duration = Duration::from_millis(20);

const SLEEPS: usize = 4;

/// A lone processor fires consecutive sleeps, each on time.
#[test]
fn a_lone_processor_fires_consecutive_sleeps() {
    let bootfs = PathBuf::from(
        std::env::var_os("HELIOS_BOOTFS")
            .expect("HELIOS_BOOTFS must name the kernel-prebuild payload"),
    );
    let machine = HostedMachine::new(&HostedConfig::for_test(1, bootfs));
    let processor = ProcessorId::new(0);
    let (jobs_tx, jobs) = unbounded::<Box<dyn FnOnce(&'static Kernel<HostedCpu>) + Send>>();
    let (registered_tx, registered) = bounded(1);

    let thread_machine = machine.clone();
    thread::Builder::new()
        .name("helios-timer-test-p0".to_owned())
        .spawn(move || {
            let machine = thread_machine;
            machine.register_thread(processor, thread::current());
            registered_tx.send(()).unwrap();
            thread::park();

            let cpu = HostedCpu::new(processor, machine.clone());
            // The host machine reports no NUMA description: one node
            // holds the processor and all of the reserved heap.
            helios_kernel::prime_bootstrap_allocator(
                machine.bootstrap_memory_regions(),
                &SingleNode::new(1),
                processor,
            );
            let platform = Platform::new(HostedConsole::new(), cpu.clone())
                .with_topology(
                    ProcessorTopology::start_all_secondaries(processor, 1)
                        .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
                )
                .with_dma_model(DmaModel::Translated)
                .with_devices(DeviceInventory::new());
            let kernel: &'static Kernel<HostedCpu> =
                Box::leak(Box::new(helios_kernel::init(platform)));
            cpu.install_timer(kernel.timer());
            loop {
                while let Ok(job) = jobs.try_recv() {
                    job(kernel);
                }
                if kernel.run_until_stalled_with_stats().progress_count() == 0 {
                    kernel.park_until_work();
                }
            }
        })
        .expect("the processor thread starts");

    registered
        .recv_timeout(PATIENCE)
        .expect("the processor thread did not register");
    machine.wake_processor(processor);

    let (elapsed_tx, elapsed) = bounded(SLEEPS);
    jobs_tx
        .send(Box::new(move |kernel| {
            let timer = kernel.timer();
            kernel.spawn_local_detached(async move {
                for _ in 0..SLEEPS {
                    let started = Instant::now();
                    timer.sleep_for(SLEEP).await;
                    elapsed_tx.send(started.elapsed()).unwrap();
                }
            });
        }))
        .unwrap();
    machine.wake_processor(processor);

    for sleep in 0..SLEEPS {
        let took = elapsed
            .recv_timeout(PATIENCE)
            .unwrap_or_else(|_| panic!("sleep {sleep} never fired on a lone processor"));
        assert!(took >= SLEEP, "sleep {sleep} fired early after {took:?}");
    }
}
