use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use futures::Stream;
use futures::stream;
use tokio::time::{self, Instant as TokioInstant, MissedTickBehavior};
use wasmtime::StoreContextMut;
use wasmtime::component::{Destination, Linker, StreamProducer, StreamReader, StreamResult};

use crate::debugger_bindings::bindings as debugger_bindings;
use crate::init_program::StoreData;
use crate::observer_buffer::matches_filter;
use crate::program_bindings::bindings as program_bindings;

/// Adds the hosted observer syscalls that are needed by the embedded debugger.
///
/// `wasmtime::component::bindgen` currently lowers free functions returning a
/// `stream<T>` through `Host`, not `HostWithStore`, so the generated trait
/// never receives a `StoreContextMut`. We therefore wire these imports
/// manually, which keeps stream creation in the `func_wrap_async` closure where
/// the store is available.
pub(crate) fn add_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    add_stats_to_linker(linker)?;
    add_instances_to_linker(linker)?;
    add_tracing_to_linker(linker)?;
    Ok(())
}

fn add_stats_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/stats@0.1.0")?;
    instance.func_wrap_async("snapshot", |caller, (): ()| {
        Box::new(async move { Ok((snapshot_sample(caller.data()),)) })
    })?;
    instance.func_wrap_async("subscribe", |mut caller, (period,): (u64,)| {
        Box::new(async move {
            let reader = StreamReader::new(&mut caller, StatsStreamProducer::new(period))?;
            Ok((reader,))
        })
    })?;
    Ok(())
}

fn add_tracing_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/tracing@0.1.0")?;
    instance.func_wrap_async(
        "recent",
        |caller, (filter, limit): (program_bindings::helios::system::tracing::Filter, u32)| {
            Box::new(async move { Ok((caller.data().observer.recent(&filter, limit),)) })
        },
    )?;
    instance.func_wrap_async(
        "subscribe",
        |mut caller, (filter,): (program_bindings::helios::system::tracing::Filter,)| {
            Box::new(async move {
                let receiver = caller.data().observer.subscribe();
                let stream = stream::unfold(receiver, move |mut receiver| {
                    let filter = filter.clone();
                    async move {
                        loop {
                            match receiver.recv().await {
                                Ok(event) if matches_filter(&event, &filter) => {
                                    return Some((event, receiver));
                                }
                                Ok(_) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    return None;
                                }
                            }
                        }
                    }
                });
                let reader = StreamReader::new(&mut caller, PipeProducer::new(stream))?;
                Ok((reader,))
            })
        },
    )?;
    Ok(())
}

fn add_instances_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/instances@0.1.0")?;
    instance.func_wrap_async("snapshot", |caller, (): ()| {
        Box::new(async move {
            Ok((caller
                .data()
                .instance_registry
                .snapshot(caller.data().boot_now_nanos())
                .into_iter()
                .map(convert_instance)
                .collect::<Vec<_>>(),))
        })
    })?;
    Ok(())
}

pub(crate) fn snapshot_sample(
    store: &StoreData,
) -> program_bindings::helios::system::stats::Sample {
    let uptime = store.boot_now_nanos();
    program_bindings::helios::system::stats::Sample {
        timestamp: uptime,
        uptime,
        processors: program_bindings::helios::system::stats::Processors {
            configured: store.processor_count,
            online: store.processor_count,
            utilization: (0..store.processor_count)
                .map(|id| program_bindings::helios::system::stats::Processor { id, busy: 0 })
                .collect(),
        },
        memory: program_bindings::helios::system::stats::Memory {
            total_bytes: 0,
            available_bytes: 0,
            pressure: program_bindings::helios::system::stats::MemoryPressure::Nominal,
        },
    }
}

fn convert_instance(
    instance: helios_kernel::InstanceSnapshot,
) -> debugger_bindings::helios::system::instances::Instance {
    debugger_bindings::helios::system::instances::Instance {
        id: instance.id.raw(),
        name: instance.name,
        started_at: instance.started_at,
        uptime: instance.uptime,
        memory_bytes: instance.memory_bytes,
        cpu_busy: instance.cpu_busy,
    }
}

struct StatsStreamProducer {
    interval: time::Interval,
}

impl StatsStreamProducer {
    fn new(period_nanos: u64) -> Self {
        assert!(period_nanos != 0, "stats subscribe period must be non-zero");

        let period = Duration::from_nanos(period_nanos);
        let mut interval = time::interval_at(TokioInstant::now() + period, period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self { interval }
    }
}

impl StreamProducer<StoreData> for StatsStreamProducer {
    type Item = program_bindings::helios::system::stats::Sample;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        match Pin::new(&mut self.interval).poll_tick(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => {
                destination.set_buffer(Some(snapshot_sample(store.data())));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

struct PipeProducer<S>(S);

impl<S> PipeProducer<S> {
    fn new(stream: S) -> Self {
        Self(stream)
    }
}

impl<T, S> StreamProducer<StoreData> for PipeProducer<S>
where
    T: Send + Sync + wasmtime::component::Lower + 'static,
    S: Stream<Item = T> + Send + 'static,
{
    type Item = T;
    type Buffer = Option<T>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let stream = unsafe { self.as_mut().map_unchecked_mut(|value| &mut value.0) };
        match stream.poll_next(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(Some(item)) => {
                destination.set_buffer(Some(item));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}
