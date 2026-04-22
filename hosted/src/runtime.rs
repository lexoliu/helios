use std::io::{Read, Write};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant as StdInstant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_hal::{DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology};

use crate::config::HostedConfig;
use crate::console::HostedConsole;
use crate::cpu::HostedCpu;
use crate::host_fs::HostedFileSystem;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

type HostedRuntimeState = helios_kernel::HostRuntimeState<HostedCpu, HostedFileSystem>;

/// Shared debug state across all hosted processor threads.
static DEBUG_STATE: OnceLock<HostedRuntimeState> = OnceLock::new();

/// Shared serial I/O mutex for debug transport over stdin/stdout.
static SERIAL_INPUT: Mutex<Option<std::io::Stdin>> = Mutex::new(None);
static SERIAL_OUTPUT: Mutex<Option<std::io::Stdout>> = Mutex::new(None);

fn init_serial() {
    let _ = SERIAL_INPUT.lock().unwrap().insert(std::io::stdin());
    let _ = SERIAL_OUTPUT.lock().unwrap().insert(std::io::stdout());
}

fn read_debug_serial(max_bytes: u32) -> Vec<u8> {
    let mut guard = SERIAL_INPUT.lock().unwrap();
    let stdin = guard.as_mut().expect("serial not initialized");
    let mut buf = vec![0u8; max_bytes as usize];
    match stdin.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

fn write_debug_serial(bytes: &[u8]) {
    let mut guard = SERIAL_OUTPUT.lock().unwrap();
    if let Some(stdout) = guard.as_mut() {
        let _ = stdout.write_all(bytes);
        let _ = stdout.flush();
    }
}

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
    pub(crate) fn new(config: &HostedConfig) -> Arc<Self> {
        let (timer_tx, timer_rx) = crossbeam_channel::unbounded();
        let started_at = StdInstant::now();
        let machine = Arc::new(Self {
            processor_count: config.processor_count(),
            bootstrap_processor: config.bootstrap_processor(),
            started_at,
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
            return;
        }

        slot.thread
            .get()
            .unwrap_or_else(|| {
                panic!(
                    "processor {} thread has not registered before start",
                    processor.id()
                )
            })
            .unpark();
    }

    pub(crate) fn wake_processor(&self, processor: ProcessorId) {
        let slot = self.slot(processor);
        if let Some(thread) = slot.thread.get() {
            thread.unpark();
        }
    }

    pub(crate) fn set_deadline(&self, processor: ProcessorId, deadline: Instant) {
        let _ = self.timer_tx.send(TimerCommand {
            processor,
            deadline: Some(deadline),
        });
    }

    pub(crate) fn register_thread(&self, processor: ProcessorId, thread: Thread) {
        self.slot(processor)
            .thread
            .set(thread)
            .unwrap_or_else(|_| panic!("processor {} thread was registered twice", processor.id()));
    }

    pub(crate) fn bootstrap_memory_regions(
        &self,
        processor: ProcessorId,
    ) -> core::iter::Once<MemoryRegion> {
        if processor == self.bootstrap_processor {
            core::iter::once(self.heap.as_memory_region())
        } else {
            core::iter::once(NonNull::from(&[] as &[u8]))
        }
    }

    fn now_ticks(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_nanos())
            .unwrap_or_else(|err| panic!("hosted uptime exceeds u64 nanos: {err}"))
    }

    fn slot(&self, processor: ProcessorId) -> &ProcessorSlot {
        &self.slots[usize::from(processor.id())]
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

            thread::park();

            let console = HostedConsole::new();
            let cpu = HostedCpu::new(processor, machine.clone());
            let memory_regions = machine.bootstrap_memory_regions(processor);
            let mut devices = DeviceInventory::new().with_debug_serial();
            if config.init_wasi_root().is_some() {
                devices = devices.with_host_share();
            }
            let platform = Platform::new(console, memory_regions, cpu.clone())
                .with_topology(
                    ProcessorTopology::start_all_secondaries(
                        machine.bootstrap_processor(),
                        machine.processor_count(),
                    )
                    .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
                )
                .with_dma_model(DmaModel::Translated)
                .with_devices(devices);
            let kernel = helios_kernel::init(platform);
            let debug_state = shared_debug_state(&machine);
            if processor == machine.bootstrap_processor() {
                init_serial();

                // Install host filesystem if configured.
                if let Some(root) = config.init_wasi_root() {
                    debug_state.install_host_fs_service(HostedFileSystem::new(root));
                }

                // Install component host program service (same as riscv/x86).
                helios_kernel::install_component_host_program_service(&kernel, &cpu, &debug_state);

                // Start non-bootstrap processors for component host topology.
                for p in helios_kernel::component_host_processors_to_start(
                    machine.processor_count(),
                    machine.bootstrap_processor(),
                ) {
                    cpu.start_processor(p);
                }
            }

            helios_kernel::run_component_host_processor_forever(
                cpu,
                kernel,
                debug_state,
                read_debug_serial,
                write_debug_serial,
            );
        })
        .unwrap_or_else(|err| panic!("failed to spawn processor {}: {err}", processor.id()))
}

fn shared_debug_state(machine: &HostedMachine) -> HostedRuntimeState {
    DEBUG_STATE
        .get_or_init(|| {
            helios_kernel::RuntimeState::new(
                machine.timer_frequency(),
                machine.processor_count(),
                machine.now_ticks(),
            )
        })
        .clone()
}

fn wait_for_processor_registration(machine: &HostedMachine, ready_rx: &Receiver<ProcessorId>) {
    for expected in 0..machine.processor_count() {
        let processor = ready_rx.recv().unwrap_or_else(|err| {
            panic!(
                "processor registration channel closed after {} registrations: {err}",
                expected
            )
        });
        tracing::info!("processor {} registered", processor.id());
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
            backing: vec![0u8; bytes].into_boxed_slice(),
        }
    }

    fn as_memory_region(&self) -> MemoryRegion {
        NonNull::from(self.backing.as_ref())
    }
}

fn spawn_timer_thread(machine: Arc<HostedMachine>, rx: Receiver<TimerCommand>) {
    thread::Builder::new()
        .name("helios-timer".to_owned())
        .spawn(move || {
            let mut deadlines = vec![None::<Instant>; machine.processor_count()];

            loop {
                let next_deadline = deadlines.iter().filter_map(|&d| d).min();

                let command = if let Some(deadline) = next_deadline {
                    let now = machine.now();
                    if deadline <= now {
                        fire_expired_deadlines(&machine, &mut deadlines);
                        continue;
                    }
                    let ticks_remaining = deadline.ticks().saturating_sub(now.ticks());
                    let nanos = ticks_remaining;
                    match rx.recv_timeout(Duration::from_nanos(nanos)) {
                        Ok(cmd) => cmd,
                        Err(RecvTimeoutError::Timeout) => {
                            fire_expired_deadlines(&machine, &mut deadlines);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match rx.recv() {
                        Ok(cmd) => cmd,
                        Err(_) => return,
                    }
                };

                let slot = usize::from(command.processor.id());
                deadlines[slot] = command.deadline;
            }
        })
        .unwrap_or_else(|err| panic!("failed to spawn timer thread: {err}"));
}

fn fire_expired_deadlines(machine: &HostedMachine, deadlines: &mut [Option<Instant>]) {
    let now = machine.now();
    for (slot, deadline) in deadlines.iter_mut().enumerate() {
        if let Some(d) = *deadline {
            if d <= now {
                *deadline = None;
                machine.wake_processor(ProcessorId::new(slot as u16));
            }
        }
    }
}
