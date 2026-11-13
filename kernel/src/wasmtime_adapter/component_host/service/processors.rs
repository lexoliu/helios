//! `helios:system/processors` for the component host.
//!
//! The interface is the WIT face of [`ProcessorLifecycle`]: the sets are
//! read off the lifecycle, and a transition is the lifecycle's own call
//! with its result mapped case for case. Nothing here decides anything
//! about a processor.
//!
//! # Concurrency contract
//!
//! `sets` is lock-free: one atomic load per possible processor, on the
//! task that owns the store. A transition takes the lifecycle handle out
//! of the store and gives the store back before it awaits, so the
//! instance's other calls run while the target processor tears down or
//! comes up; the lifecycle serialises transitions machine-wide.
//!
//! # The result mapping
//!
//! Every lifecycle error and refusal has its own WIT case, and the
//! mapping below matches each kernel type exhaustively with no
//! catch-all. A case the kernel adds is therefore a compile error here
//! until `processors.wit` names it, and from there the generated
//! bindings carry it to every guest and to the inspector protocol.

use alloc::vec::Vec;

use helios_hal::cpu::ProcessorId;
use wasmtime::component::{Accessor, HasSelf, Linker};

use crate::wasmtime_adapter::bindings::processors::bindings::helios::system::processors as processors_wit;
use crate::{
    ComponentHostNetwork, OfflineError, OfflineRefusal, OnlineError, PinnedTasks,
    ProcessorLifecycle,
};

use super::super::StoreData;

/// Register `helios:system/processors` in a linker.
///
/// Every program linker gets it, for the reason the audio interface
/// does: linkage is cached per binary while authority belongs to the
/// instance. Reading the sets needs no right, since `stats` already
/// reports the online count and each processor's load; a transition
/// needs the instance's control-processors right and is `denied`
/// without it.
pub(in crate::wasmtime_adapter::component_host) fn add_processors_to_linker<
    CpuImpl,
    Net,
    StorageImpl,
>(
    linker: &mut Linker<StoreData<CpuImpl, Net, StorageImpl>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    processors_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>(
        linker,
        |state| state,
    )
}

impl<CpuImpl, Net, StorageImpl> processors_wit::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn sets(&mut self) -> wasmtime::Result<processors_wit::ProcessorSets> {
        Ok(processor_sets(&self.spawner().processor_control()))
    }
}

impl<CpuImpl, Net, StorageImpl, U> processors_wit::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    async fn offline(
        accessor: &Accessor<U, Self>,
        processor: processors_wit::ProcessorId,
    ) -> wasmtime::Result<Result<(), processors_wit::OfflineError>> {
        let control = accessor.with(|mut access| {
            let data = access.get();
            data.process_authority()
                .derive_control_processors_cap()
                .map(|_| data.spawner().processor_control())
        });
        let Ok(control) = control else {
            return Ok(Err(processors_wit::OfflineError::Denied));
        };
        Ok(offline(&control, processor).await)
    }

    async fn online(
        accessor: &Accessor<U, Self>,
        processor: processors_wit::ProcessorId,
    ) -> wasmtime::Result<Result<(), processors_wit::OnlineError>> {
        let control = accessor.with(|mut access| {
            let data = access.get();
            data.process_authority()
                .derive_control_processors_cap()
                .map(|_| data.spawner().processor_control())
        });
        let Ok(control) = control else {
            return Ok(Err(processors_wit::OnlineError::Denied));
        };
        Ok(online(&control, processor).await)
    }
}

/// The possible and online sets as `lifecycle` reports them now.
fn processor_sets(lifecycle: &impl ProcessorLifecycle) -> processors_wit::ProcessorSets {
    let possible = lifecycle.possible_processors();
    let online = (0..possible)
        .map(processor_index)
        .filter(|&processor| lifecycle.is_online(processor))
        .map(|processor| u32::from(processor.id()))
        .collect::<Vec<_>>();
    processors_wit::ProcessorSets {
        possible: possible_count(possible),
        bootstrap: u32::from(lifecycle.bootstrap_processor().id()),
        online,
    }
}

async fn offline(
    lifecycle: &impl ProcessorLifecycle,
    processor: processors_wit::ProcessorId,
) -> Result<(), processors_wit::OfflineError> {
    // An index past a processor identifier's range names no possible
    // processor, which is the lifecycle's own answer for every index
    // past the possible set.
    let Ok(processor) = u16::try_from(processor) else {
        return Err(processors_wit::OfflineError::NotPossible(possible_count(
            lifecycle.possible_processors(),
        )));
    };
    lifecycle
        .offline(ProcessorId::new(processor))
        .await
        .map_err(offline_error)
}

async fn online(
    lifecycle: &impl ProcessorLifecycle,
    processor: processors_wit::ProcessorId,
) -> Result<(), processors_wit::OnlineError> {
    let Ok(processor) = u16::try_from(processor) else {
        return Err(processors_wit::OnlineError::NotPossible(possible_count(
            lifecycle.possible_processors(),
        )));
    };
    lifecycle
        .online(ProcessorId::new(processor))
        .await
        .map_err(online_error)
}

fn offline_error(error: OfflineError) -> processors_wit::OfflineError {
    use processors_wit::OfflineError as Wire;

    match error {
        OfflineError::NotPossible {
            processor: _,
            possible,
        } => Wire::NotPossible(possible_count(possible)),
        OfflineError::Bootstrap { processor: _ } => Wire::Bootstrap,
        OfflineError::CurrentProcessor { processor: _ } => Wire::CurrentProcessor,
        OfflineError::NotOnline { processor: _ } => Wire::NotOnline,
        OfflineError::Refused(refusal) => Wire::Refused(offline_refusal(refusal)),
    }
}

fn offline_refusal(refusal: OfflineRefusal) -> processors_wit::OfflineRefusal {
    match refusal {
        OfflineRefusal::PinnedTasks(PinnedTasks { processor: _, live }) => {
            processors_wit::OfflineRefusal::PinnedTasks(
                u64::try_from(live).expect("a live task count fits u64"),
            )
        }
    }
}

fn online_error(error: OnlineError) -> processors_wit::OnlineError {
    use processors_wit::OnlineError as Wire;

    match error {
        OnlineError::NotPossible {
            processor: _,
            possible,
        } => Wire::NotPossible(possible_count(possible)),
        OnlineError::NotOffline { processor: _ } => Wire::NotOffline,
    }
}

/// The size of the possible set, which a processor identifier bounds.
fn possible_count(possible: usize) -> u32 {
    u32::try_from(possible).unwrap_or_else(|_| {
        panic!("{possible} possible processors do not fit the u16 processor identifiers")
    })
}

fn processor_index(index: usize) -> ProcessorId {
    ProcessorId::new(u16::try_from(index).unwrap_or_else(|_| {
        panic!("possible processor {index} does not fit a processor identifier")
    }))
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use futures_lite::future::block_on;
    use helios_hal::cpu::ProcessorId;
    use spin::Mutex;

    use super::{offline, online, processor_sets, processors_wit};
    use crate::{OfflineError, OfflineRefusal, OnlineError, PinnedTasks, ProcessorLifecycle};

    /// A lifecycle that answers each transition with the next scripted
    /// result and records what it was asked.
    struct ScriptedLifecycle {
        possible: usize,
        online: Vec<bool>,
        offline_results: Mutex<Vec<Result<(), OfflineError>>>,
        online_results: Mutex<Vec<Result<(), OnlineError>>>,
        asked: Mutex<Vec<ProcessorId>>,
    }

    impl ScriptedLifecycle {
        fn new(online: &[bool]) -> Self {
            Self {
                possible: online.len(),
                online: online.to_vec(),
                offline_results: Mutex::new(Vec::new()),
                online_results: Mutex::new(Vec::new()),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProcessorLifecycle for ScriptedLifecycle {
        fn possible_processors(&self) -> usize {
            self.possible
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn is_online(&self, processor: ProcessorId) -> bool {
            self.online[usize::from(processor.id())]
        }

        async fn offline(&self, processor: ProcessorId) -> Result<(), OfflineError> {
            self.asked.lock().push(processor);
            self.offline_results.lock().remove(0)
        }

        async fn online(&self, processor: ProcessorId) -> Result<(), OnlineError> {
            self.asked.lock().push(processor);
            self.online_results.lock().remove(0)
        }
    }

    #[test]
    fn the_sets_list_the_online_processors_in_order() {
        let lifecycle = ScriptedLifecycle::new(&[true, false, true, true]);
        assert_eq!(
            processor_sets(&lifecycle),
            processors_wit::ProcessorSets {
                possible: 4,
                bootstrap: 0,
                online: alloc::vec![0, 2, 3],
            }
        );
    }

    #[test]
    fn every_offline_result_keeps_its_case_on_the_wire() {
        use processors_wit::OfflineError as Wire;

        let p3 = ProcessorId::new(3);
        let lifecycle = ScriptedLifecycle::new(&[true; 4]);
        *lifecycle.offline_results.lock() = alloc::vec![
            Ok(()),
            Err(OfflineError::NotPossible {
                processor: p3,
                possible: 4,
            }),
            Err(OfflineError::Bootstrap { processor: p3 }),
            Err(OfflineError::CurrentProcessor { processor: p3 }),
            Err(OfflineError::NotOnline { processor: p3 }),
            Err(OfflineError::Refused(OfflineRefusal::PinnedTasks(
                PinnedTasks {
                    processor: p3,
                    live: 2,
                },
            ))),
        ];
        let answers = (0..6)
            .map(|_| block_on(offline(&lifecycle, 3)))
            .collect::<Vec<_>>();
        assert_eq!(
            answers,
            [
                Ok(()),
                Err(Wire::NotPossible(4)),
                Err(Wire::Bootstrap),
                Err(Wire::CurrentProcessor),
                Err(Wire::NotOnline),
                Err(Wire::Refused(processors_wit::OfflineRefusal::PinnedTasks(
                    2
                ))),
            ]
        );
        assert_eq!(*lifecycle.asked.lock(), [p3; 6]);
    }

    #[test]
    fn every_online_result_keeps_its_case_on_the_wire() {
        use processors_wit::OnlineError as Wire;

        let p2 = ProcessorId::new(2);
        let lifecycle = ScriptedLifecycle::new(&[true, true, false, true]);
        *lifecycle.online_results.lock() = alloc::vec![
            Ok(()),
            Err(OnlineError::NotPossible {
                processor: p2,
                possible: 4,
            }),
            Err(OnlineError::NotOffline { processor: p2 }),
        ];
        let answers = (0..3)
            .map(|_| block_on(online(&lifecycle, 2)))
            .collect::<Vec<_>>();
        assert_eq!(
            answers,
            [Ok(()), Err(Wire::NotPossible(4)), Err(Wire::NotOffline)]
        );
    }

    /// An index no processor identifier can hold is not possible, and
    /// the lifecycle is never asked about it.
    #[test]
    fn an_index_past_the_identifier_range_is_not_possible() {
        let lifecycle = ScriptedLifecycle::new(&[true; 4]);
        let past = u32::from(u16::MAX) + 1;
        assert_eq!(
            block_on(offline(&lifecycle, past)),
            Err(processors_wit::OfflineError::NotPossible(4))
        );
        assert_eq!(
            block_on(online(&lifecycle, past)),
            Err(processors_wit::OnlineError::NotPossible(4))
        );
        assert!(lifecycle.asked.lock().is_empty());
    }
}
