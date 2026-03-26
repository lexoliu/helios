use std::fmt::{self, Write};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::Thread;
use std::time::{Duration, Instant as StdInstant};

use helios_hal::Platform;
use helios_hal::cpu::{Cpu, HartId, Instant};
use helios_hal::memory::MemoryRegion;
use helios_kernel::{init, yield_now};

const HOSTED_HEAP_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct HostedCpu {
    started_at: StdInstant,
    main_thread: Thread,
    timer_tx: Sender<StdInstant>,
}

impl HostedCpu {
    fn new() -> Self {
        let started_at = StdInstant::now();
        let main_thread = std::thread::current();
        let (timer_tx, timer_rx) = mpsc::channel();
        let timer_thread = main_thread.clone();
        std::thread::spawn(move || timer_thread_main(timer_rx, timer_thread));

        Self {
            started_at,
            main_thread,
            timer_tx,
        }
    }
}

fn timer_thread_main(timer_rx: Receiver<StdInstant>, main_thread: Thread) {
    while let Ok(mut deadline) = timer_rx.recv() {
        loop {
            let now = StdInstant::now();
            if now >= deadline {
                main_thread.unpark();
                break;
            }

            let timeout = deadline.saturating_duration_since(now);
            match timer_rx.recv_timeout(timeout) {
                Ok(next_deadline) => deadline = next_deadline,
                Err(RecvTimeoutError::Timeout) => {
                    main_thread.unpark();
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

impl Cpu for HostedCpu {
    fn current_hart(&self) -> HartId {
        HartId::new(0)
    }

    fn hart_count(&self) -> usize {
        1
    }

    fn bootstrap_hart(&self) -> HartId {
        HartId::new(0)
    }

    fn park_current(&self) {
        std::thread::park();
    }

    fn start_hart(&self, _hart: HartId) {}

    fn now(&self) -> Instant {
        Instant::new(self.started_at.elapsed().as_nanos() as u64)
    }

    fn timer_frequency(&self) -> u64 {
        1_000_000_000
    }

    fn set_deadline(&self, deadline: Instant) {
        let absolute_deadline = self
            .started_at
            .checked_add(Duration::from_nanos(deadline.ticks()))
            .expect("hosted timer deadline overflow");
        self.timer_tx
            .send(absolute_deadline)
            .expect("hosted timer thread stopped unexpectedly");
        self.main_thread.unpark();
    }

    fn shutdown(&self) -> ! {
        std::process::exit(0);
    }

    fn reboot(&self) -> ! {
        std::process::exit(1);
    }
}

struct StdoutConsole;

impl Write for StdoutConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        use std::io::Write as _;

        let mut stdout = std::io::stdout().lock();
        stdout.write_all(s.as_bytes()).map_err(|_| fmt::Error)?;
        stdout.flush().map_err(|_| fmt::Error)
    }
}

pub fn main() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("non-string panic payload");
        helios_kernel::panic_log_message(message, info.location());
    }));

    let heap = vec![0; HOSTED_HEAP_SIZE].into_boxed_slice();
    let heap = Box::leak(heap);
    let memory_regions = [MemoryRegion::from(heap)];
    let kernel = init(Platform::new(
        StdoutConsole,
        memory_regions,
        HostedCpu::new(),
    ));
    let spawner = kernel.spawner();
    let timer = kernel.timer();
    kernel.spawn_detached(async move {
        let joined = spawner
            .spawn(async move {
                timer.sleep_for(Duration::from_millis(10)).await;
                yield_now().await;
                2usize + 2
            })
            .await;
        tracing::info!("Executor join result={joined}");
    });
    let local_message: Rc<str> = Rc::from("spawn_local works");
    kernel.spawn_local_detached(async move {
        yield_now().await;
        tracing::info!("{local_message}");
    });
    kernel.run();
}
