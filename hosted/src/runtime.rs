use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant as StdInstant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use helios_hal::Platform;
use helios_hal::cpu::{Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;

use crate::config::HostedConfig;
use crate::console::HostedConsole;
use crate::cpu::HostedCpu;
use crate::init_program;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

pub struct HostedRuntime {
    machine: Arc<HostedMachine>,
    config: HostedConfig,
}

pub(crate) struct HostedMachine {
    processor_count: usize,
    bootstrap_processor: ProcessorId,
    started_at: StdInstant,
    heap: HeapReservation,
    slots: Box<[ProcessorSlot]>,
    timer_tx: Sender<TimerCommand>,
}

struct ProcessorSlot {
    thread: OnceLock<Thread>,
    started: AtomicBool,
}

struct HeapReservation {
    backing: Box<[u8]>,
}

struct TimerCommand {
    processor: ProcessorId,
    deadline: Option<Instant>,
}

impl HostedRuntime {
    pub fn new(config: HostedConfig) -> Self {
        Self {
            machine: HostedMachine::new(&config),
            config,
        }
    }

    pub fn run(self) -> ! {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(self.machine.processor_count());
        let mut bootstrap_handle = None;

        for processor_index in 0..self.machine.processor_count() {
            let processor = ProcessorId::new(processor_index as u16);
            let handle = spawn_processor_thread(
                self.machine.clone(),
                self.config.clone(),
                processor,
                ready_tx.clone(),
            );

            if processor == self.machine.bootstrap_processor() {
                bootstrap_handle = Some(handle);
            }
        }

        drop(ready_tx);
        wait_for_processor_registration(&self.machine, &ready_rx);
        self.machine
            .wake_processor(self.machine.bootstrap_processor());

        let bootstrap_handle =
            bootstrap_handle.expect("bootstrap processor thread was not created");
        bootstrap_handle
            .join()
            .unwrap_or_else(|_| panic!("bootstrap processor thread panicked unexpectedly"));

        panic!("bootstrap processor thread returned unexpectedly");
    }
}

impl HostedMachine {
    fn new(config: &HostedConfig) -> Arc<Self> {
        let (timer_tx, timer_rx) = crossbeam_channel::unbounded();
        let machine = Arc::new(Self {
            processor_count: config.processor_count(),
            bootstrap_processor: config.bootstrap_processor(),
            started_at: StdInstant::now(),
            heap: HeapReservation::new(config.heap_bytes()),
            slots: (0..config.processor_count())
                .map(|_| ProcessorSlot::new())
                .collect(),
            timer_tx,
        });

        spawn_timer_thread(machine.clone(), timer_rx);
        machine
    }

    pub(crate) fn processor_count(&self) -> usize {
        self.processor_count
    }

    pub(crate) fn bootstrap_processor(&self) -> ProcessorId {
        self.bootstrap_processor
    }

    pub(crate) fn now(&self) -> Instant {
        Instant::new(self.now_ticks())
    }

    pub(crate) fn timer_frequency(&self) -> u64 {
        NANOS_PER_SECOND
    }

    pub(crate) fn start_processor(&self, processor: ProcessorId) {
        assert!(
            processor != self.bootstrap_processor,
            "bootstrap processor {} cannot be started twice",
            processor.id()
        );

        let slot = self.slot(processor);
        if slot.started.swap(true, Ordering::AcqRel) {
            panic!("processor {} was started more than once", processor.id());
        }

        self.unpark(processor);
    }

    pub(crate) fn wake_processor(&self, processor: ProcessorId) {
        self.unpark(processor);
    }

    pub(crate) fn set_deadline(&self, processor: ProcessorId, deadline: Instant) {
        let deadline = (deadline.ticks() != u64::MAX).then_some(deadline);
        self.timer_tx
            .send(TimerCommand {
                processor,
                deadline,
            })
            .unwrap_or_else(|err| panic!("failed to send timer command: {err}"));
    }

    fn register_thread(&self, processor: ProcessorId, thread: Thread) {
        self.slot(processor)
            .thread
            .set(thread)
            .unwrap_or_else(|_| panic!("processor {} registered twice", processor.id()));
    }

    fn bootstrap_memory_regions(&self, processor: ProcessorId) -> Option<MemoryRegion> {
        (processor == self.bootstrap_processor).then(|| self.heap.region())
    }

    fn now_ticks(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_nanos())
            .expect("hosted monotonic clock does not fit into u64 nanoseconds")
    }

    fn slot(&self, processor: ProcessorId) -> &ProcessorSlot {
        self.slots
            .get(processor.id() as usize)
            .unwrap_or_else(|| panic!("processor {} is out of range", processor.id()))
    }

    fn unpark(&self, processor: ProcessorId) {
        self.slot(processor)
            .thread
            .get()
            .unwrap_or_else(|| panic!("processor {} thread is not registered", processor.id()))
            .unpark();
    }
}

impl ProcessorSlot {
    fn new() -> Self {
        Self {
            thread: OnceLock::new(),
            started: AtomicBool::new(false),
        }
    }
}

impl HeapReservation {
    fn new(bytes: usize) -> Self {
        Self {
            backing: vec![0; bytes].into_boxed_slice(),
        }
    }

    fn region(&self) -> MemoryRegion {
        let pointer = self.backing.as_ptr() as *mut u8;
        let length = self.backing.len();
        let slice = std::ptr::slice_from_raw_parts_mut(pointer, length);

        unsafe { NonNull::new_unchecked(slice) }
    }
}

fn spawn_processor_thread(
    machine: Arc<HostedMachine>,
    config: HostedConfig,
    processor: ProcessorId,
    ready_tx: Sender<ProcessorId>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("helios-hosted-p{}", processor.id()))
        .spawn(move || {
            machine.register_thread(processor, thread::current());
            ready_tx
                .send(processor)
                .unwrap_or_else(|err| panic!("failed to report processor readiness: {err}"));

            // The thread exists before the kernel starts so bootstrap can bring
            // processors online with the same start primitive it uses on hardware.
            thread::park();

            let console = HostedConsole::new();
            let cpu = HostedCpu::new(processor, machine.clone());
            let memory_regions = machine.bootstrap_memory_regions(processor);
            let kernel = helios_kernel::init(Platform::new(console, memory_regions, cpu));
            if processor == machine.bootstrap_processor() {
                init_program::spawn(&kernel, &config);
            }
            kernel.run();
        })
        .unwrap_or_else(|err| panic!("failed to spawn processor {}: {err}", processor.id()))
}

fn wait_for_processor_registration(machine: &HostedMachine, ready_rx: &Receiver<ProcessorId>) {
    for expected in 0..machine.processor_count() {
        let processor = ready_rx.recv().unwrap_or_else(|err| {
            panic!(
                "processor registration channel closed after {} registrations: {err}",
                expected
            )
        });
        let _ = processor;
    }
}

fn spawn_timer_thread(machine: Arc<HostedMachine>, timer_rx: Receiver<TimerCommand>) {
    thread::Builder::new()
        .name("helios-hosted-timer".to_owned())
        .spawn(move || timer_loop(machine, timer_rx))
        .unwrap_or_else(|err| panic!("failed to spawn hosted timer thread: {err}"));
}

fn timer_loop(machine: Arc<HostedMachine>, timer_rx: Receiver<TimerCommand>) -> ! {
    let mut deadlines = vec![None; machine.processor_count()];

    loop {
        fire_due_deadlines(&machine, &mut deadlines);

        match next_deadline(&deadlines) {
            Some(deadline) => {
                let now = machine.now_ticks();
                if deadline <= now {
                    continue;
                }

                let timeout = Duration::from_nanos(deadline - now);
                match timer_rx.recv_timeout(timeout) {
                    Ok(command) => apply_timer_command(&mut deadlines, command),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        panic!("hosted timer command channel disconnected")
                    }
                }
            }
            None => {
                let command = timer_rx
                    .recv()
                    .unwrap_or_else(|_| panic!("hosted timer command channel disconnected"));
                apply_timer_command(&mut deadlines, command);
            }
        }
    }
}

fn apply_timer_command(deadlines: &mut [Option<Instant>], command: TimerCommand) {
    let slot = deadlines
        .get_mut(command.processor.id() as usize)
        .unwrap_or_else(|| {
            panic!(
                "timer update for invalid processor {}",
                command.processor.id()
            )
        });
    *slot = command.deadline;
}

fn fire_due_deadlines(machine: &HostedMachine, deadlines: &mut [Option<Instant>]) {
    let now = machine.now_ticks();

    for (index, deadline) in deadlines.iter_mut().enumerate() {
        if deadline.is_some_and(|deadline| deadline.ticks() <= now) {
            *deadline = None;
            machine.wake_processor(ProcessorId::new(index as u16));
        }
    }
}

fn next_deadline(deadlines: &[Option<Instant>]) -> Option<u64> {
    deadlines.iter().flatten().map(Instant::ticks).min()
}
