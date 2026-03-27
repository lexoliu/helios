#![no_std]
extern crate alloc;
mod compute_pool;
mod executor;
mod log;
mod program;
mod task;
mod timer;

pub use compute_pool::{
    ComputePool, ComputePoolConfig, ComputePoolSnapshot, ComputePriority,
    SubmitError as ComputeSubmitError,
};
pub use executor::{JoinHandle, LocalJoinHandle, Spawner};
pub use helios_hal::Platform;
pub use task::{YieldNow, yield_now};
pub use timer::{Sleep, Timer};

use core::sync::atomic::{AtomicU8, Ordering};
use core::time::Duration;

use buddy_system_allocator::LockedHeap;
use executor::Executor;
use helios_hal::cpu::{Cpu, HartId, Instant};
use helios_hal::memory::MemoryRegion;

const HEAP_ORDER: usize = 32;
const BOOT_UNINITIALIZED: u8 = 0;
const BOOT_INITIALIZING: u8 = 1;
const BOOT_READY: u8 = 2;

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: LockedHeap<HEAP_ORDER> = LockedHeap::empty();
static BOOT_STATE: AtomicU8 = AtomicU8::new(BOOT_UNINITIALIZED);

pub struct Kernel<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    executor: Executor,
    timer: Timer<CpuImpl>,
}

impl<CpuImpl: Cpu + Clone> Kernel<CpuImpl> {
    pub fn spawner(&self) -> Spawner<CpuImpl> {
        self.executor.spawner(self.cpu.clone())
    }

    pub fn timer(&self) -> Timer<CpuImpl> {
        self.timer.clone()
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: core::future::Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawner().spawn(future)
    }

    pub fn spawn_detached<Fut>(&self, future: Fut)
    where
        Fut: core::future::Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawner().spawn_detached(future);
    }

    pub fn spawn_local<Fut>(&self, future: Fut) -> LocalJoinHandle<Fut::Output>
    where
        Fut: core::future::Future + 'static,
        Fut::Output: 'static,
    {
        self.spawner().spawn_local(future)
    }

    pub fn spawn_local_detached<Fut>(&self, future: Fut)
    where
        Fut: core::future::Future + 'static,
        Fut::Output: 'static,
    {
        self.spawner().spawn_local_detached(future);
    }

    pub fn sleep_until(&self, deadline: Instant) -> Sleep<CpuImpl> {
        self.timer.sleep_until(deadline)
    }

    pub fn sleep_for(&self, duration: Duration) -> Sleep<CpuImpl> {
        self.timer.sleep_for(duration)
    }

    pub fn run_until_stalled(&self) -> usize {
        let mut progress = 0;

        loop {
            let fired = self.timer.fire_expired();
            let ran = self.executor.run_until_stalled();

            if fired == 0 && ran == 0 {
                return progress;
            }

            progress += fired + ran;
        }
    }

    pub fn run(&self) -> ! {
        loop {
            if self.run_until_stalled() != 0 {
                continue;
            }

            self.cpu.park_current();
        }
    }
}

pub fn init<Console, CpuImpl, Regions>(
    platform: Platform<Console, CpuImpl, Regions>,
) -> Kernel<CpuImpl>
where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu + Clone,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    let Platform {
        console,
        cpu,
        memory_regions,
    } = platform;
    let current_hart = cpu.current_hart();

    if current_hart == cpu.bootstrap_hart() {
        bootstrap_init(console, memory_regions, &cpu);
    } else {
        wait_for_bootstrap(&cpu);
    }

    let kernel = Kernel {
        timer: Timer::new(cpu.clone()),
        cpu,
        executor: Executor::new(),
    };

    let hart_id = current_hart.id();
    kernel.spawn_detached(async move {
        tracing::info!("Hart online hart={hart_id}");
    });

    kernel
}

fn init_allocator<Regions>(memory_regions: Regions)
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    for mut region in memory_regions {
        let region = unsafe { region.as_mut() };
        let start = region.as_mut_ptr() as usize;
        let end = start + region.len();
        unsafe {
            ALLOCATOR.lock().add_to_heap(start, end);
        }
    }
}

fn bootstrap_init<Console, CpuImpl, Regions>(
    console: Console,
    memory_regions: Regions,
    cpu: &CpuImpl,
) where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    match BOOT_STATE.compare_exchange(
        BOOT_UNINITIALIZED,
        BOOT_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(state) => panic!("bootstrap hart observed invalid boot state {state}"),
    }

    init_allocator(memory_regions);
    log::init_logger(console);
    tracing::info!(
        "Kernel initialized on bootstrap hart={}",
        cpu.bootstrap_hart().id()
    );
    tracing::info!("Kernel is ready\n\n{}", include_str!("welcome.txt"));

    BOOT_STATE.store(BOOT_READY, Ordering::Release);

    for hart in 0..cpu.hart_count() {
        let hart = HartId::new(hart as u16);
        if hart != cpu.bootstrap_hart() {
            cpu.start_hart(hart);
        }
    }
}

fn wait_for_bootstrap<CpuImpl: Cpu>(cpu: &CpuImpl) {
    loop {
        if BOOT_STATE.load(Ordering::Acquire) == BOOT_READY {
            return;
        }
        cpu.park_current();
    }
}

pub fn panic_log(info: &core::panic::PanicInfo) {
    panic_log_message(info.message(), info.location());
}

pub fn panic_log_message(
    message: impl core::fmt::Display,
    location: Option<&core::panic::Location<'_>>,
) {
    if let Some(location) = location {
        tracing::error!(
            "Kernel panic: {} ({}:{}:{})",
            message,
            location.file(),
            location.line(),
            location.column()
        );
        return;
    }

    tracing::error!("Kernel panic: {}", message);
}
