//! Runner and supervisor for the `compositor` kernel plugin.
//!
//! `helios:system/surface` does not compose anything; it drops a
//! [`SurfaceRequest`] into the `compositor` provider slot and waits.
//! This module is what sits on the other end of that slot: it
//! instantiates `bin/compositor` — an ordinary user-mode wasm component,
//! with the ordinary isolation model — in its own store, calls the
//! plugin's `helios:system/compositor.run` as a task of that store, and
//! serves the surface calls concurrently with it.
//!
//! # Lifecycle
//!
//! Instantiation is eager, unlike the HTTP plugin's. A compositor is not
//! a service that answers requests: it *is* the desktop, it claims the
//! display and every input device the moment it starts, and a machine
//! that waits for a first request before drawing would boot to a blank
//! scanout. So the supervisor builds an instance at startup and rebuilds
//! it whenever it dies.
//!
//! Whether the machine can hold a desktop at all is decided once, here,
//! from the devices the backend brought up before the program service
//! was installed. A machine with no display or no input device never
//! grows one, so the plugin is not started on it: `helios:system/surface`
//! reports `unavailable` exactly as it does on an image that ships no
//! compositor, and nothing runs in the background. The alternative, a
//! compositor that is built, refused by the display and rebuilt every
//! `RESTART_DELAY`, is a periodic instantiate-and-tear-down on the
//! bootstrap processor for the life of the kernel (#379).
//!
//! A death is a death whatever it looked like from inside: `run`
//! returning is as much the end of the desktop as a trap or an OOM kill,
//! so both come back here as an error and the supervisor rebuilds. What
//! the machine shows in between is the blank scanout the display service
//! leaves when a claim is released, which is what makes the restart
//! visible from a capture rather than only from a log.
//!
//! Every window the dead instance was composing dies with it: the pages
//! belong to the clients and stay theirs, but the identities are retired
//! and the next call a client makes on one is answered `gone`.
//!
//! # Concurrency
//!
//! Three things run inside the one store at once: `run`, the request
//! loop, and the drain that takes back what dying clients handed over.
//! `run` is dispatched with [`Accessor::spawn`] so the other two are not
//! behind it; the supervisor task itself is `spawn_local`, so it stays
//! on the bootstrap processor alongside the store it owns.

use super::*;

use crate::component::ComponentRuntimeState;
use crate::surface::{
    SURFACE_REQUEST_QUEUE_DEPTH, SurfaceCreate, SurfaceId, SurfaceRequest, SurfaceService,
    SurfaceServiceError,
};
use crate::wasmtime_adapter::bindings::compositor::bindings::{CompositorHost, exports};
use crate::{ProcessAuthority, ProviderReceiver, provider_channel};
use thiserror::Error;
use wasmtime::component::{Accessor, AccessorTask, HasSelf, ResourceTable};

/// Bootfs path of the plugin. Absent on a kernel image built without it,
/// in which case `helios:system/surface` reports `unavailable`.
pub(super) const COMPOSITOR_PLUGIN_PATH: &str = "/bin/compositor";

/// Instance-registry name, so the desktop shows up in
/// `stats`/`instances` alongside the other kernel plugins.
pub(super) const COMPOSITOR_PLUGIN_INSTANCE_NAME: &str = "compositor-plugin";

/// How long the supervisor waits before rebuilding a compositor that
/// died.
///
/// A compositor that traps on its own first frame would otherwise spin
/// the executor rebuilding it, and a machine whose desktop is broken
/// still has to answer a shell. Long enough that the log is readable,
/// short enough that a restart looks instant to whoever is watching the
/// screen.
const RESTART_DELAY: Duration = Duration::from_millis(250);

/// Provision the `compositor` plugin, if this kernel image ships one.
///
/// Called once, from the bootstrap processor, right after the program
/// service is installed. Reading the artifact here rather than on first
/// use is what lets an image without the plugin answer `unavailable`
/// immediately instead of discovering the absence mid-request.
pub(super) fn install_compositor_plugin<CpuImpl, Net, HostFs>(
    service: &UserProgramService<CpuImpl, Net, HostFs>,
    exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(artifact) = read_bootfs_artifact(&exec_context.runtime_state, COMPOSITOR_PLUGIN_PATH)
    else {
        tracing::info!(
            path = COMPOSITOR_PLUGIN_PATH,
            "compositor plugin is not provisioned; helios:system/surface will report it unavailable"
        );
        return;
    };
    if let Some(refusal) = desktop_refusal(&exec_context.runtime_state) {
        tracing::info!(
            path = COMPOSITOR_PLUGIN_PATH,
            %refusal,
            "compositor plugin is provisioned but this machine cannot hold a desktop; \
             helios:system/surface will report it unavailable"
        );
        return;
    }

    let (sender, receiver) = provider_channel(SURFACE_REQUEST_QUEUE_DEPTH);
    exec_context
        .runtime_state
        .compositor()
        .install(sender)
        .unwrap_or_else(|error| panic!("compositor provider was installed twice: {error}"));

    let spawner = exec_context.spawner();
    let service = service.clone();
    spawner.spawn_local_detached(run_compositor_supervisor(
        service,
        exec_context,
        artifact,
        receiver,
    ));
}

/// Why this machine cannot hold a desktop.
///
/// Decided from the services the backend installed at boot, which is
/// the same answer the compositor would get from `Display::claim` and
/// `Device::claim`; the difference is that the answer is known before
/// anything is instantiated, and it never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
enum DesktopRefusal {
    #[error("this machine has no display device")]
    NoDisplay,
    #[error("this machine has no input device")]
    NoInput,
}

/// Whether the machine can hold a desktop, from the devices it has.
fn desktop_refusal(runtime_state: &impl ComponentRuntimeState) -> Option<DesktopRefusal> {
    if runtime_state.display_service().is_none() {
        return Some(DesktopRefusal::NoDisplay);
    }
    let input_devices = runtime_state
        .input_service()
        .map_or(0, |service| service.device_count());
    (input_devices == 0).then_some(DesktopRefusal::NoInput)
}

/// Own the desktop for the lifetime of the kernel, rebuilding it when it
/// dies.
async fn run_compositor_supervisor<CpuImpl, Net, HostFs>(
    service: UserProgramService<CpuImpl, Net, HostFs>,
    exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
    artifact: Bytes,
    receiver: ProviderReceiver<SurfaceRequest>,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let registry = exec_context.runtime_state.surface_service();
    let timer = exec_context.timer.clone();
    loop {
        let outcome = run_compositor_once(&service, &exec_context, &artifact, &receiver).await;
        // Whatever it was, the desktop is over: the display and the
        // input devices go back to the kernel when the store drops, the
        // scanouts blank, and every window the instance was composing is
        // retired under its client.
        registry.retire_compositor();
        match outcome {
            Err(error) if plugin_runtime_should_be_recycled(&error) => {
                tracing::warn!(
                    target: "helios_kernel::supervisor",
                    ?error,
                    "the compositor died; rebuilding it"
                );
            }
            Ok(()) => {
                tracing::warn!(
                    target: "helios_kernel::supervisor",
                    "the compositor returned from run; rebuilding it"
                );
            }
            Err(error) => {
                tracing::error!(
                    target: "helios_kernel::supervisor",
                    ?error,
                    "the compositor failed unrecoverably; the desktop is now unavailable"
                );
                return;
            }
        }
        timer.sleep_for(RESTART_DELAY).await;
    }
}

/// Build one compositor instance and serve the desktop with it until it
/// dies.
async fn run_compositor_once<CpuImpl, Net, HostFs>(
    service: &UserProgramService<CpuImpl, Net, HostFs>,
    exec_context: &ProgramExecContext<CpuImpl, Net, HostFs>,
    artifact: &Bytes,
    receiver: &ProviderReceiver<SurfaceRequest>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started_at = exec_context
        .runtime_state
        .uptime_nanos(exec_context.cpu.now().ticks());
    let instance = exec_context.instance_registry.register_with_policy(
        COMPOSITOR_PLUGIN_INSTANCE_NAME,
        started_at,
        crate::OomPolicy::KernelPlugin,
    );
    let instance_id = instance.id();

    let payload = trusted_bootfs_payload(artifact)?;
    let instance_pre = service.load_precompiled_component(
        payload,
        exec_context.write_serial,
        started_at,
        &phases::Timeline::disabled(),
    )?;

    let mut store = crate::wasmtime_adapter::store_with_state(
        service.inner.engine.raw(),
        StoreData::<CpuImpl, Net, HostFs>::new(
            ResourceTable::new(),
            exec_context.cpu.clone(),
            exec_context.timer.clone(),
            // A kernel plugin is kernel infrastructure: its tasks come
            // out of the arena's kernel reserve, so user-mode load
            // cannot starve the desktop.
            exec_context
                .spawner
                .instance_spawner(crate::TaskFunding::Kernel),
            exec_context.runtime_state.clone(),
            exec_context.instance_registry.clone(),
            instance,
            None,
            DebugFileSystem::new(exec_context.runtime_state.clone()),
            alloc::vec![String::from(COMPOSITOR_PLUGIN_PATH)],
            Vec::new(),
            // Same authority the system components get: the compositor
            // spawns the shell it draws, and it is bootfs-provisioned
            // and signed like they are.
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
    crate::wasmtime_adapter::component_host::record_linear_memory(&mut store, &wasm_instance);
    let host =
        CompositorHost::new(&mut store, &wasm_instance).map_err(map_program_runtime_error)?;
    let guest = host.helios_system_compositor().clone();

    let registry = exec_context.runtime_state.surface_service();
    // From here on this instance is the compositor: it is the one that
    // may `deliver` input to somebody else's window, and the one whose
    // death retires every window on the desktop.
    registry.set_compositor(instance_id);
    tracing::info!(
        target: "helios_kernel::supervisor",
        instance = instance_id.raw(),
        "compositor online"
    );

    let serve_registry = registry.clone();
    let serve_guest = guest.clone();
    store
        .run_concurrent(async move |accessor| {
            // The desktop itself, as a task of this store, so the calls
            // into it are not queued behind it.
            accessor.spawn(RunDesktop {
                guest: guest.clone(),
                _marker: core::marker::PhantomData,
            })?;
            accessor.spawn(DrainReturnedSurfaces {
                guest: guest.clone(),
                registry: serve_registry.clone(),
                _marker: core::marker::PhantomData,
            })?;
            while let Some(request) = receiver.recv().await {
                serve_request(accessor, &serve_guest, &serve_registry, request).await?;
            }
            Ok::<(), wasmtime::Error>(())
        })
        .await
        .and_then(|result| result)
        .map_err(map_program_runtime_error)
}

/// Serve one surface call against the live compositor.
async fn serve_request<CpuImpl, Net, HostFs>(
    accessor: &Accessor<StoreData<CpuImpl, Net, HostFs>>,
    guest: &exports::helios::system::compositor::Guest,
    registry: &SurfaceService,
    request: SurfaceRequest,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match request {
        SurfaceRequest::Create(create) => serve_create(accessor, guest, registry, create).await,
        SurfaceRequest::Commit { id, region, reply } => {
            let answer = guest
                .call_commit(
                    accessor,
                    id.raw(),
                    exports::helios::system::compositor::Rect {
                        x: region.x,
                        y: region.y,
                        width: region.width,
                        height: region.height,
                    },
                )
                .await?;
            let _ = reply.send(answer.map_err(from_guest_error));
            Ok(())
        }
        SurfaceRequest::Destroy { id } => destroy_surface(accessor, guest, id).await,
    }
}

/// Give the compositor a view of one client's pages and tell it about
/// the window they hold.
async fn serve_create<CpuImpl, Net, HostFs>(
    accessor: &Accessor<StoreData<CpuImpl, Net, HostFs>>,
    guest: &exports::helios::system::compositor::Guest,
    registry: &SurfaceService,
    create: SurfaceCreate,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let SurfaceCreate {
        id,
        geometry,
        physical,
        reply,
    } = create;
    // The view first: the compositor is told where the client's frame
    // buffer is in its own memory, so it cannot be told before it can
    // reach it.
    let view = accessor.with(|mut access| {
        let data = access.get();
        let (surfaces, window) = data.device.surfaces_mut()?;
        surfaces.map_view(registry, window, id, physical)
    });
    let view = match view {
        Ok(view) => view,
        Err(error) => {
            let _ = reply.send(Err(error));
            return Ok(());
        }
    };

    let answer = guest
        .call_create(
            accessor,
            id.raw(),
            exports::helios::system::compositor::Rect {
                x: 0,
                y: 0,
                width: geometry.width,
                height: geometry.height,
            },
            exports::helios::system::compositor::Placement {
                offset: view.offset,
                length: view.bytes,
            },
        )
        .await?;
    match answer {
        Ok(()) => {
            let _ = reply.send(Ok(()));
        }
        Err(error) => {
            // The compositor would not take the window, so the view goes
            // back before the client is told: an unmapped page is one
            // the compositor provably cannot read.
            accessor.with(|mut access| access.get().device.unmap_surface_view(id));
            let _ = reply.send(Err(from_guest_error(error)));
        }
    }
    Ok(())
}

/// Take one window off the desktop and drop the compositor's view of it.
async fn destroy_surface<CpuImpl, Net, HostFs>(
    accessor: &Accessor<StoreData<CpuImpl, Net, HostFs>>,
    guest: &exports::helios::system::compositor::Guest,
    id: SurfaceId,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    guest.call_destroy(accessor, id.raw()).await?;
    accessor.with(|mut access| access.get().device.unmap_surface_view(id));
    Ok(())
}

/// How the compositor's own refusal reads to the client that asked.
const fn from_guest_error(
    error: exports::helios::system::compositor::Error,
) -> SurfaceServiceError {
    use exports::helios::system::compositor::Error as GuestError;
    match error {
        GuestError::Unavailable => SurfaceServiceError::Unavailable,
        GuestError::NoCompositor => SurfaceServiceError::NoCompositor,
        GuestError::UnsupportedSize => SurfaceServiceError::UnsupportedSize,
        GuestError::TooManySurfaces => SurfaceServiceError::TooManySurfaces,
        GuestError::OutOfBounds => SurfaceServiceError::OutOfBounds,
        GuestError::WindowExhausted => SurfaceServiceError::WindowExhausted,
        GuestError::OutOfMemory => SurfaceServiceError::OutOfMemory,
        GuestError::Gone => SurfaceServiceError::Gone,
        GuestError::NotTheCompositor => SurfaceServiceError::NotTheCompositor,
    }
}

/// The store's type parameters, carried by a task that names them
/// without holding a value of any of them.
type StoreTypeMarker<CpuImpl, Net, HostFs> =
    core::marker::PhantomData<fn() -> (CpuImpl, Net, HostFs)>;

/// The desktop itself: one `helios:system/compositor.run` that lasts as
/// long as the instance does.
struct RunDesktop<CpuImpl, Net, HostFs> {
    guest: exports::helios::system::compositor::Guest,
    _marker: StoreTypeMarker<CpuImpl, Net, HostFs>,
}

impl<CpuImpl, Net, HostFs>
    AccessorTask<StoreData<CpuImpl, Net, HostFs>, HasSelf<StoreData<CpuImpl, Net, HostFs>>>
    for RunDesktop<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn run(
        self,
        accessor: &Accessor<StoreData<CpuImpl, Net, HostFs>>,
    ) -> wasmtime::Result<()> {
        let outcome = self.guest.call_run(accessor).await?;
        // A compositor that returned is a desktop that stopped, whatever
        // it says about why. Reporting it as an error is what ends the
        // concurrent run and hands the supervisor its rebuild.
        Err(match outcome {
            Ok(()) => wasmtime::Error::msg("the compositor returned from run"),
            Err(error) => wasmtime::Error::msg(alloc::format!(
                "the compositor gave up the desktop: {:?}",
                error
            )),
        })
    }
}

/// The drain that takes back what dying clients handed over.
struct DrainReturnedSurfaces<CpuImpl, Net, HostFs> {
    guest: exports::helios::system::compositor::Guest,
    registry: SurfaceService,
    _marker: StoreTypeMarker<CpuImpl, Net, HostFs>,
}

impl<CpuImpl, Net, HostFs>
    AccessorTask<StoreData<CpuImpl, Net, HostFs>, HasSelf<StoreData<CpuImpl, Net, HostFs>>>
    for DrainReturnedSurfaces<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn run(
        self,
        accessor: &Accessor<StoreData<CpuImpl, Net, HostFs>>,
    ) -> wasmtime::Result<()> {
        let registry = self.registry;
        // Armed before the first look, and re-armed as part of every
        // completed wait: an arena handed back between a drain and a
        // park is one this task is owed.
        let mut waiter = registry.returns_waiter();
        loop {
            for returned in registry.take_returned() {
                for id in &returned.ids {
                    // The window goes off the desktop and the view of
                    // its pages goes with it, both before the arena is
                    // dropped: the client's pages go back to its pool
                    // only once nothing here can reach them.
                    destroy_surface(accessor, &self.guest, *id).await?;
                }
                drop(returned);
            }
            core::future::poll_fn(|cx| registry.poll_returns(cx, &mut waiter)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DesktopRefusal, desktop_refusal};
    use crate::test_support::TestRuntimeState;

    /// A bench-lane or smoke boot has no display device. The plugin is
    /// not started there, instead of being rebuilt every restart delay
    /// for the life of the kernel (#379).
    #[test]
    fn a_machine_without_a_display_never_starts_the_compositor() {
        let runtime_state = TestRuntimeState::default();
        assert_eq!(
            desktop_refusal(&runtime_state),
            Some(DesktopRefusal::NoDisplay)
        );
    }
}
