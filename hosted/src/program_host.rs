use std::thread;

use futures_lite::future::block_on;
use helios_kernel::{InstanceRegistry, ProgramRuntime, ProgramRuntimeConfig, ProgramService};

use crate::config::HostedConfig;
use crate::cpu::HostedCpu;

const WORKER_STACK_SIZE: usize = 512 * 1024;

pub fn create_program_service(
    config: &HostedConfig,
    clock_cpu: HostedCpu,
    instance_registry: InstanceRegistry,
) -> ProgramService<HostedCpu> {
    let worker_count = config.processor_count().saturating_sub(2).max(1);
    let max_memory_bytes = config.heap_bytes().max(worker_count * WORKER_STACK_SIZE);
    let runtime = ProgramRuntime::new(ProgramRuntimeConfig::new(
        worker_count,
        WORKER_STACK_SIZE,
        max_memory_bytes,
    ))
    .unwrap_or_else(|error| panic!("failed to create hosted program runtime: {error}"));
    let service = ProgramService::new(runtime, clock_cpu.clone(), instance_registry);

    for worker in 0..service.compile_worker_count() {
        let service = service.clone();
        let cpu = clock_cpu.clone();
        thread::Builder::new()
            .name(format!("helios-program-worker-{worker}"))
            .spawn(move || run_worker(service, cpu))
            .unwrap_or_else(|error| {
                panic!("failed to spawn hosted program worker {worker}: {error}")
            });
    }

    service
}

fn run_worker(service: ProgramService<HostedCpu>, cpu: HostedCpu) -> ! {
    block_on(async move {
        loop {
            if service.run_next_on(&cpu) {
                continue;
            }
            service.wait_for_activity().await;
        }
    });
    panic!("hosted program worker returned unexpectedly");
}
