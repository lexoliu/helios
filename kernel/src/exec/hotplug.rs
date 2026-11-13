//! Firmware-driven processor hotplug: the requests a platform's firmware
//! raises ([`ProcessorHotplug`]) delivered to the processor lifecycle.
//!
//! This is the half every backend shares. A backend turns its firmware's
//! event (an ACPI GPE on x86, a Generic Event Device on arm64) into
//! [`HotplugRequest`]s; this task brings an inserted processor online,
//! takes an ejected one offline and releases it, and reports each answer
//! back through the same backend.
//!
//! # Concurrency contract
//!
//! One task, spawned local to the bootstrap processor by
//! [`Kernel::spawn_processor_hotplug`]. The bootstrap processor never goes
//! offline, so the task is never pinned work on a departing processor and
//! never initiates a transition of the processor it runs on. Requests are
//! answered one at a time, each holding the lifecycle's transition lock
//! for the whole of its transition ([`ProcessorControl`]); an eject keeps
//! it until the backend has released the processor, so nothing restarts
//! a processor firmware is in the middle of removing. Nothing here runs
//! in interrupt context.

use helios_hal::cpu::{Cpu, current_processor};
use helios_hal::measured_boot::MeasuredBoot;
use helios_hal::processor_hotplug::{
    HotplugAction, HotplugOutcome, HotplugRequest, ProcessorHotplug,
};
use helios_hal::watchdog::Watchdog;

use super::lifecycle::{OfflineError, OnlineError, ProcessorControl, ProcessorLifecycle};
use crate::Kernel;

/// Answers every request `firmware` raises, for the life of the machine.
async fn serve<CpuImpl, Firmware>(control: ProcessorControl<CpuImpl>, mut firmware: Firmware)
where
    CpuImpl: Cpu + Clone,
    Firmware: ProcessorHotplug,
{
    loop {
        for request in firmware.requests().await {
            let outcome = answer(&control, &mut firmware, request).await;
            tracing::info!(
                processor = request.processor.id(),
                action = ?request.action,
                ?outcome,
                "Processor hotplug request answered"
            );
            firmware.report(request, outcome);
        }
    }
}

async fn answer<CpuImpl, Firmware>(
    control: &ProcessorControl<CpuImpl>,
    firmware: &mut Firmware,
    request: HotplugRequest,
) -> HotplugOutcome
where
    CpuImpl: Cpu + Clone,
    Firmware: ProcessorHotplug,
{
    let processor = request.processor;
    match request.action {
        HotplugAction::Insert => match control.online(processor).await {
            // A device check on a processor that is already running asks
            // for nothing new.
            Ok(()) | Err(OnlineError::NotOffline { .. }) => HotplugOutcome::Completed,
            Err(error @ OnlineError::NotPossible { .. }) => panic!(
                "firmware asked to insert a processor outside the possible set the backend \
                 resolved it into: {error}"
            ),
        },
        HotplugAction::Eject => {
            match control
                .remove(processor, |processor| firmware.eject(processor))
                .await
            {
                Ok(()) => HotplugOutcome::Completed,
                Err(OfflineError::Refused(_) | OfflineError::NotOnline { .. }) => {
                    HotplugOutcome::Busy
                }
                Err(OfflineError::Bootstrap { .. }) => HotplugOutcome::NotSupported,
                Err(
                    error @ (OfflineError::NotPossible { .. }
                    | OfflineError::CurrentProcessor { .. }),
                ) => panic!(
                    "processor hotplug could not eject processor {}: {error}; the request \
                     names a resolved possible processor and the hotplug task runs on the \
                     bootstrap processor",
                    processor.id()
                ),
            }
        }
    }
}

impl<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog + Clone, MeasuredBootImpl: MeasuredBoot>
    Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>
{
    /// Starts answering `firmware`'s processor hotplug requests, on a task
    /// local to the bootstrap processor. Called on the bootstrap
    /// processor.
    pub fn spawn_processor_hotplug<Firmware>(&self, firmware: Firmware)
    where
        Firmware: ProcessorHotplug + 'static,
    {
        let bootstrap = self.cpu.bootstrap_processor();
        assert_eq!(
            current_processor(),
            bootstrap,
            "processor hotplug is served from the bootstrap processor, which never goes offline"
        );
        self.spawn_local_detached(serve(self.processor_control(), firmware));
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec;
    use alloc::vec::Vec;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll};

    use futures::task::noop_waker_ref;
    use helios_hal::cpu::ProcessorId;
    use helios_hal::processor_hotplug::{
        HotplugAction, HotplugOutcome, HotplugRequest, ProcessorHotplug,
    };

    use super::answer;
    use crate::exec::Executor;
    use crate::exec::lifecycle::ProcessorControl;
    use crate::test_processor_identity;
    use crate::test_support::{RecordingSmpCpu, single_node_pool};
    use helios_hal::watchdog::ProgressCounter;

    const P0: ProcessorId = ProcessorId::new(0);
    const P1: ProcessorId = ProcessorId::new(1);

    /// Firmware that records which processors it was asked to release.
    #[derive(Default)]
    struct RecordingFirmware {
        ejected: Vec<ProcessorId>,
    }

    impl ProcessorHotplug for RecordingFirmware {
        async fn requests(&mut self) -> Vec<HotplugRequest> {
            vec![]
        }

        async fn eject(&mut self, processor: ProcessorId) {
            self.ejected.push(processor);
        }

        fn report(&mut self, _request: HotplugRequest, _outcome: HotplugOutcome) {}
    }

    fn run<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(noop_waker_ref());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the hotplug answer waited on a processor that never ran"),
        }
    }

    /// The bootstrap processor is never released, and a possible
    /// processor that never ran is released without a transition: there
    /// is nothing to take offline, and firmware gets it back at once.
    #[test]
    fn an_eject_releases_a_never_started_processor_and_refuses_the_bootstrap() {
        test_processor_identity::set(P0);
        let cpu = RecordingSmpCpu::new(0, 2);
        let executor = Executor::isolated(ProgressCounter::new(), single_node_pool(2), P0);
        executor.group().bring_processor_online(P0);
        let control = ProcessorControl::new(cpu, executor.group());
        let mut firmware = RecordingFirmware::default();

        let bootstrap = HotplugRequest {
            processor: P0,
            action: HotplugAction::Eject,
        };
        assert_eq!(
            run(answer(&control, &mut firmware, bootstrap)),
            HotplugOutcome::NotSupported
        );
        assert!(firmware.ejected.is_empty());

        let absent = HotplugRequest {
            processor: P1,
            action: HotplugAction::Eject,
        };
        assert_eq!(
            run(answer(&control, &mut firmware, absent)),
            HotplugOutcome::Completed
        );
        assert_eq!(firmware.ejected, [P1]);
    }
}
