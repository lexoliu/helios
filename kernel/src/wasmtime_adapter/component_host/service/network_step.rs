//! The component host's half of a processor's lifecycle: its packet
//! pump and the network queue pair the pump drains.
//!
//! The kernel's own lifecycle steps cannot name the network service —
//! the kernel is not generic over it — so the run loop that is (the
//! component host's) lends this step to the lifecycle
//! (`Kernel::run_until_stalled_with_steps`). It comes up after every
//! kernel owner and goes down before them.
//!
//! # Concurrency contract
//!
//! - One step per processor, built and used on that processor only: the
//!   pump handle is a local task's, `!Send`, and the `RefCell` around it
//!   is touched by the owner alone.
//! - `online` and `offline` run on the processor itself, at the top of
//!   its run loop with no task mid-poll, while the initiator of the
//!   transition holds the transition lock. The queue-pair hand-off they
//!   call is the network service's (`NetworkPacketPump`), synchronous and
//!   lock-free toward every pump and socket operation.
//! - Offline hands the pair to an online processor *before* it cancels
//!   the pump, so the pair always has a pump draining it, and the
//!   cancelled pump is dropped by the executor step that runs next — the
//!   pump is never pinned work the offline refuses on.

use core::cell::RefCell;

use helios_hal::cpu::{Cpu, ProcessorId, current_processor};
use helios_hal::measured_boot::MeasuredBoot;
use helios_hal::watchdog::Watchdog;

use crate::exec::{LocalJoinHandle, OfflineRefusal, ProcessorStep};
use crate::{ComponentHostNetwork, Kernel};

use super::HostRuntimeState;

/// This processor's packet pump and the hand-off of its queue pair.
pub(super) struct NetworkStep<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    debug_state: HostRuntimeState<CpuImpl, Net, StorageImpl>,
    /// The running pump while the processor is online.
    pump: RefCell<Option<LocalJoinHandle<()>>>,
}

impl<CpuImpl, Net, StorageImpl> NetworkStep<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    /// The processor's first bring-up: starts its pump. The routing
    /// table's boot layout already names the pair it drains, so nothing
    /// is handed over.
    pub(super) fn start<KernelCpu, WatchdogImpl, MeasuredBootImpl>(
        kernel: &Kernel<KernelCpu, WatchdogImpl, MeasuredBootImpl>,
        debug_state: &HostRuntimeState<CpuImpl, Net, StorageImpl>,
    ) -> Self
    where
        KernelCpu: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        let step = Self {
            debug_state: debug_state.clone(),
            pump: RefCell::new(None),
        };
        step.spawn_pump(kernel);
        step
    }

    /// Spawns this processor's pump as a local task.
    ///
    /// The service is installed by a device-discovery task that may not
    /// have run yet, so the task awaits the install rather than checking
    /// and giving up; the pump then drains whichever pair the routing
    /// table gives this processor, and parks while it gives none.
    fn spawn_pump<KernelCpu, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<KernelCpu, WatchdogImpl, MeasuredBootImpl>,
    ) where
        KernelCpu: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        let debug_state = self.debug_state.clone();
        let processor = current_processor();
        let pump = kernel.spawn_local(async move {
            let service = debug_state.wait_for_network_service().await;
            service.run_packet_pump(processor).await;
        });
        let previous = self.pump.replace(Some(pump));
        assert!(
            previous.is_none(),
            "processor {} started its packet pump twice",
            processor.id()
        );
    }
}

impl<CpuImpl, Net, StorageImpl> ProcessorStep for NetworkStep<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    /// Takes up a pair when the device has one that is not live, then
    /// starts the pump that drains it.
    fn online<KernelCpu, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<KernelCpu, WatchdogImpl, MeasuredBootImpl>,
    ) where
        KernelCpu: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        // Offline handed nothing off when no service was installed, and
        // a machine that has a network refuses that offline, so the
        // service is present exactly when there is a pair to take up.
        if let Some(service) = self.debug_state.network_service() {
            service.take_up_queue_pair(current_processor());
        }
        self.spawn_pump(kernel);
    }

    /// Hands the processor's pair to an online processor, then cancels
    /// the pump. Refuses, with nothing changed, while the machine's
    /// network interface is not up yet: there is no routing table to
    /// hand the pair off through.
    fn offline<KernelCpu, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<KernelCpu, WatchdogImpl, MeasuredBootImpl>,
        _successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        KernelCpu: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        let processor = current_processor();
        match self.debug_state.network_service() {
            Some(service) => service.hand_off_queue_pair(processor, kernel.online_set()),
            None if kernel.devices().has_network => {
                return Err(OfflineRefusal::NetworkStarting { processor });
            }
            None => {}
        }
        // Dropping the handle cancels the task; the executor step that
        // runs next drops its future.
        drop(self.pump.take());
        Ok(())
    }
}
