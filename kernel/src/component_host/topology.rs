use helios_hal::cpu::ProcessorId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentHostProcessorRole {
    Kernel,
    SystemComponent,
    ProgramWorker,
}

pub fn component_host_system_processor(
    bootstrap_processor: ProcessorId,
    processor_count: usize,
) -> ProcessorId {
    assert!(
        processor_count > 1,
        "system component topology requires at least two processors"
    );
    assert!(
        usize::from(bootstrap_processor.id()) < processor_count,
        "bootstrap processor {} is outside detected processor count {}",
        bootstrap_processor.id(),
        processor_count,
    );

    if bootstrap_processor.id() == 0 {
        ProcessorId::new(1)
    } else {
        ProcessorId::new(0)
    }
}

pub fn system_component_should_run_on(
    current_processor: ProcessorId,
    processor_count: usize,
    bootstrap_processor: ProcessorId,
) -> bool {
    current_processor == component_host_system_processor(bootstrap_processor, processor_count)
}

pub fn component_host_processor_role(
    current_processor: ProcessorId,
    processor_count: usize,
    bootstrap_processor: ProcessorId,
) -> ComponentHostProcessorRole {
    if current_processor == bootstrap_processor {
        return ComponentHostProcessorRole::Kernel;
    }
    if system_component_should_run_on(current_processor, processor_count, bootstrap_processor) {
        return ComponentHostProcessorRole::SystemComponent;
    }
    if processor_count > 2 {
        return ComponentHostProcessorRole::ProgramWorker;
    }
    ComponentHostProcessorRole::Kernel
}

pub fn component_host_worker_count(
    processor_count: usize,
    bootstrap_processor: ProcessorId,
) -> usize {
    (0..processor_count)
        .map(|processor| ProcessorId::new(processor as u16))
        .filter(|&processor| {
            component_host_processor_role(processor, processor_count, bootstrap_processor)
                == ComponentHostProcessorRole::ProgramWorker
        })
        .count()
}

/// Returns an iterator of processor IDs that need to be started for the
/// component-host topology. This excludes the bootstrap processor itself.
pub fn component_host_processors_to_start(
    processor_count: usize,
    bootstrap_processor: ProcessorId,
) -> impl Iterator<Item = ProcessorId> {
    (0..processor_count)
        .map(|id| ProcessorId::new(id as u16))
        .filter(move |&processor| {
            processor != bootstrap_processor
                && matches!(
                    component_host_processor_role(processor, processor_count, bootstrap_processor),
                    ComponentHostProcessorRole::SystemComponent
                        | ComponentHostProcessorRole::ProgramWorker
                )
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ComponentHostProcessorRole, component_host_processor_role,
        component_host_system_processor, component_host_worker_count,
    };
    use helios_hal::cpu::ProcessorId;

    #[test]
    fn chooses_secondary_processor_for_bootstrap_zero() {
        assert_eq!(
            component_host_system_processor(ProcessorId::new(0), 4),
            ProcessorId::new(1)
        );
    }

    #[test]
    fn chooses_processor_zero_for_non_zero_bootstrap() {
        assert_eq!(
            component_host_system_processor(ProcessorId::new(2), 4),
            ProcessorId::new(0)
        );
    }

    #[test]
    fn assigns_manual_worker_roles_after_bootstrap_and_system_processors() {
        assert_eq!(
            component_host_processor_role(ProcessorId::new(0), 4, ProcessorId::new(0)),
            ComponentHostProcessorRole::Kernel
        );
        assert_eq!(
            component_host_processor_role(ProcessorId::new(1), 4, ProcessorId::new(0)),
            ComponentHostProcessorRole::SystemComponent
        );
        assert_eq!(
            component_host_processor_role(ProcessorId::new(2), 4, ProcessorId::new(0)),
            ComponentHostProcessorRole::ProgramWorker
        );
        assert_eq!(component_host_worker_count(4, ProcessorId::new(0)), 2);
    }
}
