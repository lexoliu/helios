#![no_std]
extern crate alloc;
use core::fmt::Write;

use crate::cpu::ProcessorId;
use crate::memory::MemoryRegion;

pub mod boot;
pub mod cpu;
pub mod critical_section;
pub mod device;
pub mod entropy;
pub mod fs;
pub mod interrupt;
pub mod io;
pub mod iommu;
pub mod memory;
pub mod pmm;
pub mod resource;
pub mod rtc;
pub mod serial;
pub mod vmm;
pub mod vmm_test_harness;
pub mod watchdog;

/// Aligns `value` up to the next multiple of `align`.
///
/// Panics if the alignment would overflow.
pub const fn align_up(value: usize, align: usize) -> usize {
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(next) => next & !mask,
        None => panic!("alignment overflow"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessorStartupPolicy {
    BootstrapOnly,
    StartAllSecondaries,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorTopology {
    pub bootstrap_processor: ProcessorId,
    pub configured_processors: usize,
    pub startup_policy: ProcessorStartupPolicy,
}

impl ProcessorTopology {
    pub const fn bootstrap_only(bootstrap_processor: ProcessorId) -> Self {
        Self {
            bootstrap_processor,
            configured_processors: 1,
            startup_policy: ProcessorStartupPolicy::BootstrapOnly,
        }
    }

    pub fn start_all_secondaries(
        bootstrap_processor: ProcessorId,
        configured_processors: usize,
    ) -> Self {
        assert!(
            configured_processors != 0,
            "configured processor count must be greater than zero"
        );
        Self {
            bootstrap_processor,
            configured_processors,
            startup_policy: ProcessorStartupPolicy::StartAllSecondaries,
        }
    }

    pub const fn with_startup_policy(self, startup_policy: ProcessorStartupPolicy) -> Self {
        Self {
            startup_policy,
            ..self
        }
    }
}

/// How a device's DMA addresses relate to physical memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaModel {
    /// A device address is a physical address, and the kernel's virtual
    /// addresses are physical addresses too.
    Identity,
    /// A device address is a physical address the kernel reaches through
    /// a fixed virtual offset.
    Translated,
    /// A device address is an I/O virtual address an IOMMU translates,
    /// so a device only reaches the ranges its domain maps.
    Iommu,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceInventory {
    pub has_debug_serial: bool,
    pub has_network: bool,
    pub block_device_count: usize,
    pub has_host_share: bool,
    /// A hardware entropy device the kernel can read continuously.
    pub has_entropy_device: bool,
}

impl DeviceInventory {
    pub const fn new() -> Self {
        Self {
            has_debug_serial: false,
            has_network: false,
            block_device_count: 0,
            has_host_share: false,
            has_entropy_device: false,
        }
    }

    pub const fn with_debug_serial(self) -> Self {
        Self {
            has_debug_serial: true,
            ..self
        }
    }

    pub const fn with_network(self) -> Self {
        Self {
            has_network: true,
            ..self
        }
    }

    pub const fn with_block_devices(self, block_device_count: usize) -> Self {
        Self {
            block_device_count,
            ..self
        }
    }

    pub const fn with_host_share(self) -> Self {
        Self {
            has_host_share: true,
            ..self
        }
    }

    pub const fn with_entropy_device(self) -> Self {
        Self {
            has_entropy_device: true,
            ..self
        }
    }
}

pub struct Platform<
    Console: Write + Send + 'static,
    Cpu: cpu::Cpu,
    Regions,
    WatchdogImpl = watchdog::NoWatchdog,
> where
    Regions: IntoIterator<Item = MemoryRegion>,
    WatchdogImpl: watchdog::Watchdog,
{
    pub console: Console,
    pub cpu: Cpu,
    pub memory_regions: Regions,
    pub topology: ProcessorTopology,
    pub timer_frequency_hz: u64,
    pub dma_model: DmaModel,
    pub devices: DeviceInventory,
    pub watchdog: WatchdogImpl,
}

impl<Console: Write + Send + 'static, Cpu: cpu::Cpu, Regions> Platform<Console, Cpu, Regions>
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    pub fn new(console: Console, memory_regions: Regions, cpu: Cpu) -> Self {
        Self::with_watchdog(console, memory_regions, cpu, watchdog::NoWatchdog)
    }
}

impl<Console: Write + Send + 'static, Cpu: cpu::Cpu, Regions, WatchdogImpl>
    Platform<Console, Cpu, Regions, WatchdogImpl>
where
    Regions: IntoIterator<Item = MemoryRegion>,
    WatchdogImpl: watchdog::Watchdog,
{
    pub fn with_watchdog(
        console: Console,
        memory_regions: Regions,
        cpu: Cpu,
        watchdog: WatchdogImpl,
    ) -> Self {
        let topology = ProcessorTopology::start_all_secondaries(
            cpu.bootstrap_processor(),
            cpu.processor_count(),
        );
        let timer_frequency_hz = cpu.timer_frequency();
        Self {
            console,
            memory_regions,
            cpu,
            topology,
            timer_frequency_hz,
            dma_model: DmaModel::Identity,
            devices: DeviceInventory::new(),
            watchdog,
        }
    }

    pub fn with_topology(self, topology: ProcessorTopology) -> Self {
        Self { topology, ..self }
    }

    pub fn with_timer_frequency_hz(self, timer_frequency_hz: u64) -> Self {
        Self {
            timer_frequency_hz,
            ..self
        }
    }

    pub fn with_dma_model(self, dma_model: DmaModel) -> Self {
        Self { dma_model, ..self }
    }

    pub fn with_devices(self, devices: DeviceInventory) -> Self {
        Self { devices, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt;

    struct TestConsole;

    impl fmt::Write for TestConsole {
        fn write_str(&mut self, _s: &str) -> fmt::Result {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct TestCpu;

    impl cpu::Cpu for TestCpu {
        fn current_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn processor_count(&self) -> usize {
            4
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(1)
        }

        fn park_current(&self) {
            panic!("test CPU should not park")
        }

        fn start_processor(&self, _processor: ProcessorId) {
            panic!("test CPU should not start processors")
        }

        fn wake_processor(&self, _processor: ProcessorId) {
            panic!("test CPU should not wake processors")
        }

        fn now(&self) -> cpu::Instant {
            cpu::Instant::new(0)
        }

        fn timer_frequency(&self) -> u64 {
            24_000_000
        }

        fn set_deadline(&self, _deadline: cpu::Instant) {
            panic!("test CPU should not set deadlines")
        }

        fn publish_executable(&self, _ptr: *const u8, _len: usize) {}

        fn unpublish_executable(&self, _ptr: *const u8, _len: usize) {}

        fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
            None
        }

        fn shutdown(&self) -> ! {
            panic!("test CPU should not shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU should not reboot")
        }
    }

    fn empty_regions() -> [MemoryRegion; 0] {
        []
    }

    #[test]
    fn platform_new_captures_cpu_defaults() {
        let platform = Platform::new(TestConsole, empty_regions(), TestCpu);

        assert_eq!(
            platform.topology,
            ProcessorTopology::start_all_secondaries(ProcessorId::new(1), 4)
        );
        assert_eq!(platform.timer_frequency_hz, 24_000_000);
        assert_eq!(platform.dma_model, DmaModel::Identity);
        assert_eq!(platform.devices, DeviceInventory::new());
    }

    #[test]
    fn platform_builders_override_platform_facts() {
        let platform = Platform::new(TestConsole, empty_regions(), TestCpu)
            .with_topology(
                ProcessorTopology::start_all_secondaries(ProcessorId::new(1), 4)
                    .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
            )
            .with_timer_frequency_hz(1_000_000)
            .with_dma_model(DmaModel::Translated)
            .with_devices(
                DeviceInventory::new()
                    .with_debug_serial()
                    .with_network()
                    .with_block_devices(2)
                    .with_host_share(),
            );

        assert_eq!(
            platform.topology.startup_policy,
            ProcessorStartupPolicy::BootstrapOnly
        );
        assert_eq!(platform.timer_frequency_hz, 1_000_000);
        assert_eq!(platform.dma_model, DmaModel::Translated);
        assert_eq!(
            platform.devices,
            DeviceInventory::new()
                .with_debug_serial()
                .with_network()
                .with_block_devices(2)
                .with_host_share()
        );
    }
}
