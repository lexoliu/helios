//! Runner and supervisor for the `http-client` kernel plugin.
//!
//! `wasi:http/client.send` does not speak HTTP; it drops an [`HttpExchange`]
//! into the `http_client` provider slot and waits. This module is what sits on
//! the other end of that slot: it instantiates `bin/http-client` — an ordinary
//! user-mode wasm component, with the ordinary isolation model — in its own
//! store, and calls the plugin's `wasi:http/handler.handle` export once per
//! exchange.
//!
//! # Lifecycle
//!
//! Instantiation is lazy. The supervisor parks on the queue until the first
//! exchange arrives, so a kernel that never makes an HTTP request never pays
//! for the plugin. If the instance dies in a way that means its runtime is no
//! longer trustworthy — an OOM kill, a trap — the supervisor drops the store
//! and rebuilds on the next exchange, exactly like the compiler plugin. Any
//! exchanges that were in flight lose their response senders, which the
//! calling program observes as `error-code.internal-error` rather than a hang.
//!
//! # Concurrency
//!
//! Exchanges are dispatched with [`Accessor::spawn`], so several requests are
//! in flight inside the one plugin instance at a time; the plugin opens a
//! connection per request. The supervisor task itself is `spawn_local`, so it
//! stays on the bootstrap processor alongside the store it owns.

use super::*;

use crate::wasmtime_adapter::wasi::WasiRequest;
use crate::wasmtime_adapter::wasi::http::kernel_error_code;
use crate::wasmtime_adapter::wasi::http_bindings::{HttpHost, exports};
use crate::{HttpErrorCode, HttpExchange, ProcessAuthority, ProviderReceiver, provider_channel};
use wasmtime::component::{Accessor, AccessorTask, HasSelf, ResourceTable};

/// Bootfs path of the plugin. Absent on a kernel image built without it, in
/// which case `wasi:http/client` reports `configuration-error`.
pub(super) const HTTP_CLIENT_PLUGIN_PATH: &str = "/bin/http-client";

/// Exchanges that may be queued before `client.send` starts applying
/// backpressure to its callers.
const HTTP_PLUGIN_QUEUE_DEPTH: usize = 16;

/// Instance-registry name, so the plugin shows up in `stats`/`instances`
/// alongside the compiler plugin.
const HTTP_PLUGIN_INSTANCE_NAME: &str = "http-client-plugin";

/// Provision the `http-client` plugin, if this kernel image ships one.
///
/// Called once, from the bootstrap processor, right after the program service
/// is installed. Reading the artifact here (rather than on first use) is what
/// lets an image without the plugin answer `configuration-error` immediately
/// instead of discovering the absence mid-request.
pub(super) fn install_http_client_plugin<CpuImpl, HostFs>(
    service: &UserProgramService<CpuImpl, HostFs>,
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(artifact) = read_bootfs_artifact(&exec_context.runtime_state, HTTP_CLIENT_PLUGIN_PATH)
    else {
        tracing::info!(
            path = HTTP_CLIENT_PLUGIN_PATH,
            "http client plugin is not provisioned; wasi:http/client will report a configuration error"
        );
        return;
    };

    let (sender, receiver) = provider_channel(HTTP_PLUGIN_QUEUE_DEPTH);
    exec_context
        .runtime_state
        .http_client()
        .install(sender)
        .unwrap_or_else(|error| panic!("http client provider was installed twice: {error}"));

    let spawner = exec_context.spawner();
    let service = service.clone();
    spawner.spawn_local_detached(run_http_plugin_supervisor(
        service,
        exec_context,
        artifact,
        receiver,
    ));
}

/// Own the plugin for the lifetime of the kernel, rebuilding it when its
/// runtime stops being trustworthy.
async fn run_http_plugin_supervisor<CpuImpl, HostFs>(
    service: UserProgramService<CpuImpl, HostFs>,
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    artifact: Bytes,
    receiver: ProviderReceiver<HttpExchange>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    loop {
        // Nothing is instantiated until there is work: an idle kernel parks
        // here rather than holding a wasm instance open.
        let Some(first) = receiver.recv().await else {
            return;
        };

        match run_http_plugin_once(&service, &exec_context, &artifact, first, &receiver).await {
            Ok(()) => return,
            Err(error) if plugin_runtime_should_be_recycled(&error) => {
                tracing::warn!(
                    target: "helios_kernel::supervisor",
                    ?error,
                    "http client plugin instance died; rebuilding on the next exchange"
                );
            }
            Err(error) => {
                tracing::error!(
                    target: "helios_kernel::supervisor",
                    ?error,
                    "http client plugin failed unrecoverably; wasi:http/client is now unavailable"
                );
                return;
            }
        }
    }
}

/// Build one plugin instance and serve exchanges with it until it dies.
async fn run_http_plugin_once<CpuImpl, HostFs>(
    service: &UserProgramService<CpuImpl, HostFs>,
    exec_context: &ProgramExecContext<CpuImpl, HostFs>,
    artifact: &Bytes,
    first: HttpExchange,
    receiver: &ProviderReceiver<HttpExchange>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started_at = exec_context
        .runtime_state
        .uptime_nanos(exec_context.cpu.now().ticks());
    let instance = exec_context.instance_registry.register_with_policy(
        HTTP_PLUGIN_INSTANCE_NAME,
        started_at,
        crate::OomPolicy::KernelPlugin,
    );

    let payload = trusted_bootfs_payload(artifact)?;
    let instance_pre =
        service.load_precompiled_component(payload, exec_context.write_serial, started_at)?;

    let mut store = crate::wasmtime_adapter::store_with_state(
        service.inner.engine.raw(),
        StoreData::<CpuImpl, HostFs>::new(
            ResourceTable::new(),
            exec_context.cpu.clone(),
            exec_context.timer.clone(),
            // A kernel plugin is kernel infrastructure: its tasks come
            // out of the arena's kernel reserve, so user-mode load
            // cannot starve the plugins the kernel depends on.
            exec_context
                .spawner
                .instance_spawner(crate::TaskFunding::Kernel),
            exec_context.runtime_state.clone(),
            exec_context.instance_registry.clone(),
            instance,
            None,
            DebugFileSystem::new(exec_context.runtime_state.clone()),
            alloc::vec![String::from(HTTP_CLIENT_PLUGIN_PATH)],
            Vec::new(),
            // Same authority the system components get: the plugin needs the
            // network, and it is bootfs-provisioned and signed like they are.
            ProcessAuthority::root(),
            OutputMode::Serial,
            exec_context.read_serial,
            exec_context.write_serial,
        ),
    );

    let wasm_instance = instance_pre
        .instantiate_async(&mut store)
        .await
        .map_err(map_program_runtime_error)?;
    let handler = HttpHost::new(&mut store, &wasm_instance).map_err(map_program_runtime_error)?;
    let guest = handler.wasi_http_handler().clone();

    let mut first = Some(first);
    store
        .run_concurrent(async move |accessor| {
            let exchange = first.take().expect("the first exchange is dispatched once");
            dispatch_exchange(accessor, &guest, exchange)?;
            while let Some(exchange) = receiver.recv().await {
                dispatch_exchange(accessor, &guest, exchange)?;
            }
            Ok::<(), wasmtime::Error>(())
        })
        .await
        .and_then(|result| result)
        .map_err(map_program_runtime_error)
}

/// Hand one exchange to a background task so several can be in flight.
fn dispatch_exchange<CpuImpl, HostFs>(
    accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
    guest: &exports::wasi::http::handler::Guest,
    exchange: HttpExchange,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    accessor.spawn(HandleExchange {
        guest: guest.clone(),
        exchange,
        _marker: core::marker::PhantomData,
    })?;
    Ok(())
}

/// One `wasi:http/handler.handle` call, start to finish.
struct HandleExchange<CpuImpl, HostFs> {
    guest: exports::wasi::http::handler::Guest,
    exchange: HttpExchange,
    _marker: core::marker::PhantomData<fn() -> (CpuImpl, HostFs)>,
}

impl<CpuImpl, HostFs> AccessorTask<StoreData<CpuImpl, HostFs>, HasSelf<StoreData<CpuImpl, HostFs>>>
    for HandleExchange<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn run(self, accessor: &Accessor<StoreData<CpuImpl, HostFs>>) -> wasmtime::Result<()> {
        let Self {
            guest, exchange, ..
        } = self;
        let HttpExchange {
            head,
            body,
            response,
        } = exchange;

        // Push the request into the plugin's own table. Its headers are
        // already immutable, so the plugin reads the caller's request exactly
        // as the caller built it.
        let request =
            accessor.with(|mut access| access.get().table.push(WasiRequest::from_host(head, body)));
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                let _ = response.send(Err(internal_error(&error)));
                return Err(error.into());
            }
        };

        match guest.call_handle(accessor, request).await {
            Ok(Ok(produced)) => {
                let converted = accessor.with(|mut access| {
                    let produced = access.get().table.delete(produced)?;
                    produced.into_host_response(&mut access)
                });
                match converted {
                    Ok(http_response) => {
                        let _ = response.send(Ok(http_response));
                        Ok(())
                    }
                    Err(error) => {
                        let _ = response.send(Err(internal_error(&error)));
                        Err(error)
                    }
                }
            }
            // The plugin reported a protocol-level failure; that is a normal
            // answer, not a reason to rebuild the instance.
            Ok(Err(code)) => {
                let _ = response.send(Err(kernel_error_code(code)));
                Ok(())
            }
            // A trap leaves the store unusable, so tell the caller and let the
            // supervisor rebuild.
            Err(trap) => {
                let _ = response.send(Err(internal_error(&trap)));
                Err(trap)
            }
        }
    }
}

/// Render a host-side failure as the error code the caller receives.
fn internal_error(error: &impl core::fmt::Debug) -> HttpErrorCode {
    HttpErrorCode::InternalError(Some(alloc::format!("{error:?}")))
}
