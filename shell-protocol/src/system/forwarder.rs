use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use futures_core::Stream;
use futures_lite::StreamExt as _;
use helios_api::bindings::helios::system::sync as host_sync;
use helios_api::{serial as host_serial, stats as host_stats, tracing as host_tracing};
use wrpc_transport::{ResourceBorrow, ResourceOwn, ServeExt as _, TupleDecode, TupleEncode};

use crate::transport::Server;

use super::exports::exports::helios::system::{
    serial as remote_serial, stats as remote_stats, sync as remote_sync, tracing as remote_tracing,
};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type InvocationStream = Pin<Box<dyn Stream<Item = BoxFuture<Result<()>>> + Send + 'static>>;

pub async fn serve<R, W>(read: R, write: W) -> Result<()>
where
    R: futures_io::AsyncRead + Send + Unpin + 'static,
    W: futures_io::AsyncWrite + Send + Unpin + 'static,
{
    let wrpc = Server::new(read, write);
    let handler = Forwarder::new(host_serial::debug_port().map(Arc::new));
    let mut invocations = vec![
                bind::<_, _, _, (), (remote_stats::Sample,), _, _>(
                    &wrpc,
                    handler.clone(),
                "helios:system/stats@0.1.0",
                "snapshot",
                |handler, cx, ()| async move {
                    Ok((remote_stats::Handler::snapshot(&handler, cx).await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (remote_stats::MonoNanos,),
                (Pin<Box<dyn Stream<Item = Vec<remote_stats::Sample>> + Send>>,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/stats@0.1.0",
                "subscribe",
                |handler, cx, (period,)| async move {
                    Ok((remote_stats::Handler::subscribe(&handler, cx, period).await?,))
                },
            )
            .await?,
            bind::<_, _, _, (remote_tracing::Filter, u32), (Vec<remote_tracing::Event>,), _, _>(
                &wrpc,
                handler.clone(),
                "helios:system/tracing@0.1.0",
                "recent",
                |handler, cx, (filter, limit)| async move {
                    Ok((remote_tracing::Handler::recent(&handler, cx, filter, limit).await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (remote_tracing::Filter,),
                (Pin<Box<dyn Stream<Item = Vec<remote_tracing::Event>> + Send>>,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/tracing@0.1.0",
                "subscribe",
                |handler, cx, (filter,)| async move {
                    Ok((remote_tracing::Handler::subscribe(&handler, cx, filter).await?,))
                },
            )
            .await?,
            bind::<_, _, _, (), (Option<ResourceOwn<remote_serial::SerialPort>>,), _, _>(
                &wrpc,
                handler.clone(),
                "helios:system/serial@0.1.0",
                "debug-port",
                |handler, cx, ()| async move {
                    Ok((remote_serial::Handler::debug_port(&handler, cx).await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (ResourceBorrow<remote_serial::SerialPort>,),
                (remote_serial::SerialRights,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/serial@0.1.0",
                "serial-port.rights",
                |handler, cx, (serial_port,)| async move {
                    Ok(
                        (
                            remote_serial::HandlerSerialPort::rights(&handler, cx, serial_port)
                                .await?,
                        ),
                    )
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (ResourceBorrow<remote_serial::SerialPort>, u32),
                (BoxFuture<Bytes>,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/serial@0.1.0",
                "serial-port.read",
                |handler, cx, (serial_port, max_bytes)| async move {
                    Ok((remote_serial::HandlerSerialPort::read(
                        &handler,
                        cx,
                        serial_port,
                        max_bytes,
                    )
                    .await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (ResourceBorrow<remote_serial::SerialPort>, Bytes),
                (BoxFuture<()>,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/serial@0.1.0",
                "serial-port.write",
                |handler, cx, (serial_port, bytes)| async move {
                    Ok((
                        remote_serial::HandlerSerialPort::write(&handler, cx, serial_port, bytes)
                            .await?,
                    ))
                },
            )
            .await?,
            bind::<_, _, _, (ResourceBorrow<remote_serial::SerialPort>,), (BoxFuture<()>,), _, _>(
                &wrpc,
                handler.clone(),
                "helios:system/serial@0.1.0",
                "serial-port.flush",
                |handler, cx, (serial_port,)| async move {
                    Ok(
                        (
                            remote_serial::HandlerSerialPort::flush(&handler, cx, serial_port)
                                .await?,
                        ),
                    )
                },
            )
            .await?,
            bind::<_, _, _, (), (ResourceOwn<remote_sync::RawMutex>,), _, _>(
                &wrpc,
                handler.clone(),
                "helios:system/sync@0.1.0",
                "raw-mutex",
                |handler, cx, ()| async move {
                    Ok((remote_sync::HandlerRawMutex::new(&handler, cx).await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (ResourceBorrow<remote_sync::RawMutex>,),
                (BoxFuture<ResourceOwn<remote_sync::RawMutexGuard>>,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/sync@0.1.0",
                "raw-mutex.lock",
                |handler, cx, (raw_mutex,)| async move {
                    Ok((remote_sync::HandlerRawMutex::lock(&handler, cx, raw_mutex).await?,))
                },
            )
            .await?,
            bind::<_, _, _, (), (ResourceOwn<remote_sync::RawRwLock>,), _, _>(
                &wrpc,
                handler.clone(),
                "helios:system/sync@0.1.0",
                "raw-rw-lock",
                |handler, cx, ()| async move {
                    Ok((remote_sync::HandlerRawRwLock::new(&handler, cx).await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (ResourceBorrow<remote_sync::RawRwLock>,),
                (BoxFuture<ResourceOwn<remote_sync::RawRwLockReadGuard>>,),
                _,
                _,
            >(
                &wrpc,
                handler.clone(),
                "helios:system/sync@0.1.0",
                "raw-rw-lock.read",
                |handler, cx, (raw_rw_lock,)| async move {
                    Ok((remote_sync::HandlerRawRwLock::read(&handler, cx, raw_rw_lock).await?,))
                },
            )
            .await?,
            bind::<
                _,
                _,
                _,
                (ResourceBorrow<remote_sync::RawRwLock>,),
                (BoxFuture<ResourceOwn<remote_sync::RawRwLockWriteGuard>>,),
                _,
                _,
            >(
                &wrpc,
                handler,
                "helios:system/sync@0.1.0",
                "raw-rw-lock.write",
                |handler, cx, (raw_rw_lock,)| async move {
                    Ok((remote_sync::HandlerRawRwLock::write(&handler, cx, raw_rw_lock).await?,))
                },
                )
                .await?,
            ];
    let mut active = Vec::<BoxFuture<Result<()>>>::new();
    futures_lite::future::poll_fn(|cx| {
        loop {
            let mut progress = false;
            let mut index = 0;
            while index < active.len() {
                match active[index].as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => {
                        active.swap_remove(index);
                        progress = true;
                    }
                    Poll::Ready(Err(error)) => {
                        if is_transport_disconnect(&error) {
                            return Poll::Ready(Ok(()));
                        }
                        return Poll::Ready(Err(error));
                    }
                    Poll::Pending => {
                        index += 1;
                    }
                }
            }

            let mut stream_index = 0;
            while stream_index < invocations.len() {
                match invocations[stream_index].as_mut().poll_next(cx) {
                    Poll::Ready(Some(invocation)) => {
                        active.push(invocation);
                        progress = true;
                        stream_index += 1;
                    }
                    Poll::Ready(None) => {
                        invocations.swap_remove(stream_index);
                        progress = true;
                    }
                    Poll::Pending => {
                        stream_index += 1;
                    }
                }
            }

            if wrpc.is_closed() {
                return Poll::Ready(Ok(()));
            }

            if invocations.is_empty() {
                if active.is_empty() {
                    return Poll::Ready(Ok(()));
                }
                if progress {
                    continue;
                }
            }

            if progress {
                continue;
            }
            return Poll::Pending;
        }
    })
    .await
}

async fn bind<R, W, H, Params, Results, F, Fut>(
    wrpc: &Server<R, W>,
    handler: H,
    instance: &'static str,
    func: &'static str,
    serve: F,
) -> Result<InvocationStream>
where
    R: futures_io::AsyncRead + Send + Unpin + 'static,
    W: futures_io::AsyncWrite + Send + Unpin + 'static,
    H: Clone + Send + 'static,
    Params: TupleDecode<crate::transport::Incoming<R, W>> + Send + 'static,
    Results: TupleEncode<crate::transport::Outgoing<R, W>> + Send + 'static,
    <Params::Decoder as tokio_util::codec::Decoder>::Error:
        std::error::Error + Send + Sync + 'static,
    <Results::Encoder as tokio_util::codec::Encoder<Results>>::Error:
        std::error::Error + Send + Sync + 'static,
    F: Fn(H, (), Params) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<Results>> + Send + 'static,
{
    let invocations = wrpc
        .serve_values::<Params, Results>(instance, func, [])
        .await
        .with_context(|| format!("failed to register remote invocation {instance}.{func}"))?;
    Ok(Box::pin(invocations.map(move |invocation| {
        let handler = handler.clone();
        let serve = serve.clone();
        Box::pin(async move {
            let (cx, params, rx, tx) = invocation
                .with_context(|| format!("failed to accept remote invocation {instance}.{func}"))?;
            assert!(
                rx.is_none(),
                "unexpected async parameters for remote invocation {instance}.{func}"
            );
            let results = serve(handler, cx, params).await?;
            tx(results)
                .await
                .with_context(|| format!("failed to serve remote invocation {instance}.{func}"))
        }) as BoxFuture<Result<()>>
    })))
}

#[derive(Clone)]
struct Forwarder {
    debug_port: Option<Arc<host_serial::DebugPort>>,
    serial_ports: Arc<Mutex<ResourceTable<Arc<host_serial::DebugPort>>>>,
    raw_mutexes: Arc<Mutex<ResourceTable<Arc<host_sync::RawMutex>>>>,
    raw_mutex_guards: Arc<Mutex<ResourceTable<host_sync::RawMutexGuard>>>,
    raw_rw_locks: Arc<Mutex<ResourceTable<Arc<host_sync::RawRwLock>>>>,
    raw_rw_lock_read_guards: Arc<Mutex<ResourceTable<host_sync::RawRwLockReadGuard>>>,
    raw_rw_lock_write_guards: Arc<Mutex<ResourceTable<host_sync::RawRwLockWriteGuard>>>,
}

struct ResourceTable<T> {
    entries: Vec<Option<T>>,
    free: Vec<u32>,
}

impl Default for Forwarder {
    fn default() -> Self {
        panic!("forwarder requires explicit construction")
    }
}

impl Forwarder {
    fn new(debug_port: Option<Arc<host_serial::DebugPort>>) -> Self {
        Self {
            debug_port,
            serial_ports: Arc::new(Mutex::new(ResourceTable::default())),
            raw_mutexes: Arc::new(Mutex::new(ResourceTable::default())),
            raw_mutex_guards: Arc::new(Mutex::new(ResourceTable::default())),
            raw_rw_locks: Arc::new(Mutex::new(ResourceTable::default())),
            raw_rw_lock_read_guards: Arc::new(Mutex::new(ResourceTable::default())),
            raw_rw_lock_write_guards: Arc::new(Mutex::new(ResourceTable::default())),
        }
    }
}

impl<T> Default for ResourceTable<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<T> ResourceTable<T> {
    fn insert(&mut self, value: T) -> u32 {
        if let Some(id) = self.free.pop() {
            self.entries[id as usize] = Some(value);
            return id;
        }

        let id = u32::try_from(self.entries.len()).expect("resource table overflowed u32 ids");
        self.entries.push(Some(value));
        id
    }

    fn get(&self, id: u32, name: &str) -> &T {
        self.entries
            .get(id as usize)
            .and_then(Option::as_ref)
            .unwrap_or_else(|| panic!("unknown remote {name} handle {id}"))
    }

    fn remove(&mut self, id: u32, name: &str) -> T {
        let value = self
            .entries
            .get_mut(id as usize)
            .and_then(Option::take)
            .unwrap_or_else(|| panic!("unknown remote {name} handle {id}"));
        self.free.push(id);
        value
    }
}

impl<Ctx: Send> remote_serial::Handler<Ctx> for Forwarder {
    async fn debug_port(&self, _cx: Ctx) -> Result<Option<ResourceOwn<remote_serial::SerialPort>>> {
        let Some(debug_port) = self.debug_port.as_ref() else {
            return Ok(None);
        };
        let id = lock_table(&self.serial_ports, "serial-port").insert(Arc::clone(debug_port));
        Ok(Some(pack_handle(id)))
    }
}

impl<Ctx: Send> remote_serial::HandlerSerialPort<Ctx> for Forwarder {
    async fn rights(
        &self,
        _cx: Ctx,
        serial_port: ResourceBorrow<remote_serial::SerialPort>,
    ) -> Result<remote_serial::SerialRights> {
        let id = unpack_borrow_handle(serial_port, "serial-port");
        let port = clone_resource(&self.serial_ports, id, "serial-port");
        Ok(convert_serial_rights(port.rights()))
    }

    async fn read(
        &self,
        _cx: Ctx,
        serial_port: ResourceBorrow<remote_serial::SerialPort>,
        max_bytes: u32,
    ) -> Result<BoxFuture<Bytes>> {
        let id = unpack_borrow_handle(serial_port, "serial-port");
        let port = clone_resource(&self.serial_ports, id, "serial-port");
        Ok(Box::pin(async move {
            Bytes::from(
                port.read(max_bytes as usize)
                    .await
                    .unwrap_or_else(|error| panic!("failed to read debug serial port: {error}")),
            )
        }))
    }

    async fn write(
        &self,
        _cx: Ctx,
        serial_port: ResourceBorrow<remote_serial::SerialPort>,
        bytes: Bytes,
    ) -> Result<BoxFuture<()>> {
        let id = unpack_borrow_handle(serial_port, "serial-port");
        let port = clone_resource(&self.serial_ports, id, "serial-port");
        Ok(Box::pin(async move {
            port.write_all(&bytes)
                .await
                .unwrap_or_else(|error| panic!("failed to write debug serial port: {error}"));
        }))
    }

    async fn flush(
        &self,
        _cx: Ctx,
        serial_port: ResourceBorrow<remote_serial::SerialPort>,
    ) -> Result<BoxFuture<()>> {
        let id = unpack_borrow_handle(serial_port, "serial-port");
        let port = clone_resource(&self.serial_ports, id, "serial-port");
        Ok(Box::pin(async move {
            port.flush()
                .await
                .unwrap_or_else(|error| panic!("failed to flush debug serial port: {error}"));
        }))
    }
}

impl<Ctx: Send> remote_sync::Handler<Ctx> for Forwarder {}

impl<Ctx: Send> remote_sync::HandlerRawMutex<Ctx> for Forwarder {
    async fn new(&self, _cx: Ctx) -> Result<ResourceOwn<remote_sync::RawMutex>> {
        let id =
            lock_table(&self.raw_mutexes, "raw-mutex").insert(Arc::new(host_sync::RawMutex::new()));
        Ok(pack_handle(id))
    }

    async fn lock(
        &self,
        _cx: Ctx,
        raw_mutex: ResourceBorrow<remote_sync::RawMutex>,
    ) -> Result<BoxFuture<ResourceOwn<remote_sync::RawMutexGuard>>> {
        let id = unpack_borrow_handle(raw_mutex, "raw-mutex");
        let raw_mutex = clone_resource(&self.raw_mutexes, id, "raw-mutex");
        let guards = Arc::clone(&self.raw_mutex_guards);
        Ok(Box::pin(async move {
            let guard = raw_mutex.lock().await;
            let guard_id = lock_table(&guards, "raw-mutex-guard").insert(guard);
            pack_handle(guard_id)
        }))
    }
}

impl<Ctx: Send> remote_sync::HandlerRawMutexGuard<Ctx> for Forwarder {}

impl<Ctx: Send> remote_sync::HandlerRawRwLock<Ctx> for Forwarder {
    async fn new(&self, _cx: Ctx) -> Result<ResourceOwn<remote_sync::RawRwLock>> {
        let id = lock_table(&self.raw_rw_locks, "raw-rw-lock")
            .insert(Arc::new(host_sync::RawRwLock::new()));
        Ok(pack_handle(id))
    }

    async fn read(
        &self,
        _cx: Ctx,
        raw_rw_lock: ResourceBorrow<remote_sync::RawRwLock>,
    ) -> Result<BoxFuture<ResourceOwn<remote_sync::RawRwLockReadGuard>>> {
        let id = unpack_borrow_handle(raw_rw_lock, "raw-rw-lock");
        let raw_rw_lock = clone_resource(&self.raw_rw_locks, id, "raw-rw-lock");
        let guards = Arc::clone(&self.raw_rw_lock_read_guards);
        Ok(Box::pin(async move {
            let guard = raw_rw_lock.read().await;
            let guard_id = lock_table(&guards, "raw-rw-lock-read-guard").insert(guard);
            pack_handle(guard_id)
        }))
    }

    async fn write(
        &self,
        _cx: Ctx,
        raw_rw_lock: ResourceBorrow<remote_sync::RawRwLock>,
    ) -> Result<BoxFuture<ResourceOwn<remote_sync::RawRwLockWriteGuard>>> {
        let id = unpack_borrow_handle(raw_rw_lock, "raw-rw-lock");
        let raw_rw_lock = clone_resource(&self.raw_rw_locks, id, "raw-rw-lock");
        let guards = Arc::clone(&self.raw_rw_lock_write_guards);
        Ok(Box::pin(async move {
            let guard = raw_rw_lock.write().await;
            let guard_id = lock_table(&guards, "raw-rw-lock-write-guard").insert(guard);
            pack_handle(guard_id)
        }))
    }
}

impl<Ctx: Send> remote_sync::HandlerRawRwLockReadGuard<Ctx> for Forwarder {}

impl<Ctx: Send> remote_sync::HandlerRawRwLockWriteGuard<Ctx> for Forwarder {}

impl<Ctx: Send> remote_stats::Handler<Ctx> for Forwarder {
    async fn snapshot(&self, _cx: Ctx) -> Result<remote_stats::Sample> {
        Ok(convert_stats_sample(host_stats::snapshot()))
    }

    async fn subscribe(
        &self,
        _cx: Ctx,
        period: remote_stats::MonoNanos,
    ) -> Result<Pin<Box<dyn Stream<Item = Vec<remote_stats::Sample>> + Send>>> {
        let stream = wit_bindgen_wrpc::futures::stream::unfold(
            host_stats::subscribe(period),
            |mut stream| async move {
                stream
                    .next()
                    .await
                    .map(|sample| (vec![convert_stats_sample(sample)], stream))
            },
        );
        Ok(Box::pin(stream))
    }
}

impl<Ctx: Send> remote_tracing::Handler<Ctx> for Forwarder {
    async fn recent(
        &self,
        _cx: Ctx,
        filter: remote_tracing::Filter,
        limit: u32,
    ) -> Result<Vec<remote_tracing::Event>> {
        Ok(host_tracing::recent(&convert_tracing_filter(filter), limit)
            .into_iter()
            .map(convert_tracing_event)
            .collect())
    }

    async fn subscribe(
        &self,
        _cx: Ctx,
        filter: remote_tracing::Filter,
    ) -> Result<Pin<Box<dyn Stream<Item = Vec<remote_tracing::Event>> + Send>>> {
        let stream = wit_bindgen_wrpc::futures::stream::unfold(
            host_tracing::subscribe(&convert_tracing_filter(filter)),
            |mut stream| async move {
                stream
                    .next()
                    .await
                    .map(|event| (vec![convert_tracing_event(event)], stream))
            },
        );
        Ok(Box::pin(stream))
    }
}

fn lock_table<'a, T>(
    table: &'a Mutex<ResourceTable<T>>,
    name: &str,
) -> std::sync::MutexGuard<'a, ResourceTable<T>> {
    table
        .lock()
        .unwrap_or_else(|_| panic!("remote {name} table mutex poisoned"))
}

fn clone_resource<T: Clone>(table: &Mutex<ResourceTable<T>>, id: u32, name: &str) -> T {
    lock_table(table, name).get(id, name).clone()
}

fn pack_handle<T: ?Sized>(id: u32) -> ResourceOwn<T> {
    ResourceOwn::from(Bytes::copy_from_slice(&id.to_le_bytes()))
}

fn unpack_borrow_handle<T: ?Sized>(handle: ResourceBorrow<T>, name: &str) -> u32 {
    unpack_handle_bytes(Bytes::from(handle), name)
}

fn unpack_own_handle<T: ?Sized>(handle: ResourceOwn<T>, name: &str) -> u32 {
    unpack_handle_bytes(Bytes::from(handle), name)
}

fn unpack_handle_bytes(bytes: Bytes, name: &str) -> u32 {
    assert!(
        bytes.len() == 4,
        "remote {name} handle must be exactly 4 bytes, got {}",
        bytes.len()
    );
    let mut raw = [0_u8; 4];
    raw.copy_from_slice(&bytes);
    u32::from_le_bytes(raw)
}

fn convert_serial_rights(rights: host_serial::SerialRights) -> remote_serial::SerialRights {
    remote_serial::SerialRights::from_bits_retain(rights.bits())
}

fn convert_stats_sample(sample: host_stats::Sample) -> remote_stats::Sample {
    remote_stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        processors: convert_stats_processors(sample.processors),
        memory: convert_stats_memory(sample.memory),
    }
}

fn convert_stats_processors(processors: host_stats::Processors) -> remote_stats::Processors {
    remote_stats::Processors {
        configured: processors.configured,
        online: processors.online,
        utilization: processors
            .utilization
            .into_iter()
            .map(convert_stats_processor)
            .collect(),
    }
}

fn convert_stats_processor(processor: host_stats::Processor) -> remote_stats::Processor {
    remote_stats::Processor {
        id: processor.id,
        busy: processor.busy,
    }
}

fn convert_stats_memory(memory: host_stats::Memory) -> remote_stats::Memory {
    remote_stats::Memory {
        total_bytes: memory.total_bytes,
        available_bytes: memory.available_bytes,
        pressure: convert_stats_memory_pressure(memory.pressure),
    }
}

fn convert_stats_memory_pressure(
    pressure: host_stats::MemoryPressure,
) -> remote_stats::MemoryPressure {
    match pressure {
        host_stats::MemoryPressure::Nominal => remote_stats::MemoryPressure::Nominal,
        host_stats::MemoryPressure::Elevated => remote_stats::MemoryPressure::Elevated,
        host_stats::MemoryPressure::High => remote_stats::MemoryPressure::High,
        host_stats::MemoryPressure::Critical => remote_stats::MemoryPressure::Critical,
    }
}

fn convert_tracing_filter(filter: remote_tracing::Filter) -> host_tracing::Filter {
    host_tracing::Filter {
        min_level: filter.min_level.map(convert_tracing_level_from_remote),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_tracing_event(event: host_tracing::Event) -> remote_tracing::Event {
    remote_tracing::Event {
        timestamp: event.timestamp,
        level: convert_tracing_level_to_remote(event.level),
        target: event.target,
        fields: event
            .fields
            .into_iter()
            .map(convert_tracing_field)
            .collect(),
    }
}

fn convert_tracing_field(field: host_tracing::Field) -> remote_tracing::Field {
    remote_tracing::Field {
        key: field.key,
        value: convert_tracing_value(field.value),
    }
}

fn convert_tracing_value(value: host_tracing::Value) -> remote_tracing::Value {
    match value {
        host_tracing::Value::Boolean(value) => remote_tracing::Value::Boolean(value),
        host_tracing::Value::Signed64(value) => remote_tracing::Value::Signed64(value),
        host_tracing::Value::Unsigned64(value) => remote_tracing::Value::Unsigned64(value),
        host_tracing::Value::Float64(value) => remote_tracing::Value::Float64(value),
        host_tracing::Value::Text(value) => remote_tracing::Value::Text(value),
        host_tracing::Value::Blob(value) => remote_tracing::Value::Blob(value.into()),
    }
}

fn convert_tracing_level_from_remote(level: remote_tracing::Level) -> host_tracing::Level {
    match level {
        remote_tracing::Level::Error => host_tracing::Level::Error,
        remote_tracing::Level::Warn => host_tracing::Level::Warn,
        remote_tracing::Level::Info => host_tracing::Level::Info,
        remote_tracing::Level::Debug => host_tracing::Level::Debug,
        remote_tracing::Level::Trace => host_tracing::Level::Trace,
    }
}

fn convert_tracing_level_to_remote(level: host_tracing::Level) -> remote_tracing::Level {
    match level {
        host_tracing::Level::Error => remote_tracing::Level::Error,
        host_tracing::Level::Warn => remote_tracing::Level::Warn,
        host_tracing::Level::Info => remote_tracing::Level::Info,
        host_tracing::Level::Debug => remote_tracing::Level::Debug,
        host_tracing::Level::Trace => remote_tracing::Level::Trace,
    }
}

fn is_transport_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotConnected
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::Poll;

    use bytes::Bytes;
    use futures_core::Stream as _;
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
    use wrpc_transport::{ResourceBorrow, ResourceOwn};

    use super::*;

    #[derive(Clone, Copy)]
    struct SerialOnlyHandler;

    impl remote_serial::Handler<()> for SerialOnlyHandler {
        async fn debug_port(
            &self,
            _cx: (),
        ) -> Result<Option<ResourceOwn<remote_serial::SerialPort>>> {
            Ok(Some(ResourceOwn::from(Bytes::from_static(&[0, 0, 0, 0]))))
        }
    }

    impl remote_serial::HandlerSerialPort<()> for SerialOnlyHandler {
        async fn rights(
            &self,
            _cx: (),
            _serial_port: ResourceBorrow<remote_serial::SerialPort>,
        ) -> Result<remote_serial::SerialRights> {
            Ok(remote_serial::SerialRights::READ
                | remote_serial::SerialRights::WRITE
                | remote_serial::SerialRights::FLUSH)
        }

        async fn read(
            &self,
            _cx: (),
            _serial_port: ResourceBorrow<remote_serial::SerialPort>,
            _max_bytes: u32,
        ) -> Result<BoxFuture<Bytes>> {
            Ok(Box::pin(async { Bytes::new() }))
        }

        async fn write(
            &self,
            _cx: (),
            _serial_port: ResourceBorrow<remote_serial::SerialPort>,
            _bytes: Bytes,
        ) -> Result<BoxFuture<()>> {
            Ok(Box::pin(async {}))
        }

        async fn flush(
            &self,
            _cx: (),
            _serial_port: ResourceBorrow<remote_serial::SerialPort>,
        ) -> Result<BoxFuture<()>> {
            Ok(Box::pin(async {}))
        }
    }

    #[test]
    fn bind_loop_serves_debug_port_resource() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client =
                    crate::transport::Client::new(peer_read.compat(), peer_write.compat_write());

                let streams = vec![
                    bind::<_, _, _, (), (Option<ResourceOwn<remote_serial::SerialPort>>,), _, _>(
                        &server,
                        SerialOnlyHandler,
                        "helios:system/serial@0.1.0",
                        "debug-port",
                        |handler, cx, ()| async move {
                            Ok((remote_serial::Handler::debug_port(&handler, cx).await?,))
                        },
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to register test debug-port binding: {error}")
                    }),
                    bind::<
                        _,
                        _,
                        _,
                        (ResourceBorrow<remote_serial::SerialPort>,),
                        (remote_serial::SerialRights,),
                        _,
                        _,
                    >(
                        &server,
                        SerialOnlyHandler,
                        "helios:system/serial@0.1.0",
                        "serial-port.rights",
                        |handler, cx, (serial_port,)| async move {
                            Ok((remote_serial::HandlerSerialPort::rights(
                                &handler,
                                cx,
                                serial_port,
                            )
                            .await?,))
                        },
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to register test serial-port.rights binding: {error}")
                    }),
                ];
                let server_task = tokio::spawn(async move {
                    let mut invocations =
                        pin!(wit_bindgen_wrpc::futures::stream::select_all(streams));
                    let mut active = Vec::<BoxFuture<Result<()>>>::new();
                    futures_lite::future::poll_fn(|cx| {
                        loop {
                            let mut progress = false;
                            let mut index = 0;
                            while index < active.len() {
                                match active[index].as_mut().poll(cx) {
                                    Poll::Ready(Ok(())) => {
                                        active.swap_remove(index);
                                        progress = true;
                                    }
                                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                                    Poll::Pending => {
                                        index += 1;
                                    }
                                }
                            }

                            if !active.is_empty() {
                                if progress {
                                    continue;
                                }
                                return Poll::Pending;
                            }

                            match invocations.as_mut().poll_next(cx) {
                                Poll::Ready(Some(invocation)) => {
                                    progress = true;
                                    active.push(invocation);
                                }
                                Poll::Ready(None) if active.is_empty() => {
                                    return Poll::Ready(Ok(()));
                                }
                                Poll::Ready(None) | Poll::Pending if progress => continue,
                                Poll::Ready(None) | Poll::Pending => return Poll::Pending,
                            }
                        }
                    })
                    .await
                });

                let debug_port = crate::system::serial::debug_port(&client)
                    .await
                    .unwrap_or_else(|error| panic!("debug-port invocation failed: {error:#}"))
                    .unwrap_or_else(|| panic!("debug-port response must be present"));
                let rights = debug_port.rights(&client).await.unwrap_or_else(|error| {
                    panic!("serial-port.rights invocation failed: {error:#}")
                });
                assert_eq!(
                    rights,
                    crate::system::serial::SerialRights::READ
                        | crate::system::serial::SerialRights::WRITE
                        | crate::system::serial::SerialRights::FLUSH
                );

                drop(client);
                server_task
                    .await
                    .unwrap_or_else(|error| panic!("server task panicked: {error}"))
                    .unwrap_or_else(|error| panic!("server task failed: {error:#}"));
            });
    }
}
