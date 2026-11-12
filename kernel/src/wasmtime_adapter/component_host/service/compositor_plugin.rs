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
//! Every window the dead instance was composing dies with it: the frame
//! buffers belong to the clients and stay theirs — the dead store's
//! views of them are withdrawn with its view table — but the identities
//! are retired and the next call a client makes on one is answered
//! `gone`.
//!
//! # Concurrency
//!
//! Three things run inside the one store at once: `run`, the request
//! loop, and the drain that takes back every window a client let go —
//! by dying, by dropping it, or by cancelling its `create`.
//! `run` is dispatched with [`Accessor::spawn`] so the other two are not
//! behind it; the supervisor task itself is `spawn_local`, so it stays
//! on the bootstrap processor alongside the store it owns.

use super::*;

use crate::buffer::Span;
use crate::component::ComponentRuntimeState;
use crate::surface::{
    SURFACE_REQUEST_QUEUE_DEPTH, SurfaceCreate, SurfaceId, SurfaceRequest, SurfaceService,
    SurfaceServiceError,
};
use crate::wasmtime_adapter::bindings::compositor::bindings::{CompositorHost, exports};
use crate::wasmtime_adapter::component_host::{guest_memory, view_trap};
use crate::{DeviceClassRights, ProcessAuthority, ProviderReceiver, provider_channel};
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

/// The authority the compositor runs with: everything `root` grants,
/// since it spawns the shell it draws and is bootfs-provisioned and
/// signed like the system components, plus the display and the input
/// devices it draws and reads. It never claims the GPU.
fn compositor_authority() -> ProcessAuthority {
    let mut authority = ProcessAuthority::root();
    authority.grant_device_classes(DeviceClassRights::DISPLAY | DeviceClassRights::INPUT);
    authority
}

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
    artifact: BootfsBytes,
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
    artifact: &BootfsBytes,
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
        COMPOSITOR_PLUGIN_PATH,
        payload,
        exec_context.write_serial,
        started_at,
        &phases::Timeline::disabled(),
    )?;

    let (mut store, interrupts) = crate::wasmtime_adapter::store_with_state(
        &service.inner.engine,
        StoreData::<CpuImpl, Net, HostFs>::new(
            ResourceTable::new(),
            exec_context.cpu.clone(),
            exec_context.timer.clone(),
            // A kernel plugin is kernel infrastructure: its tasks come
            // out of the arena's kernel reserve, so user-mode load
            // cannot starve the desktop.
            exec_context
                .spawner
                .instance_spawner(crate::TaskFunding::Kernel, &instance),
            exec_context.runtime_state.clone(),
            exec_context.instance_registry.clone(),
            instance,
            None,
            exec_context.runtime_state.root_namespace().descend(),
            alloc::vec![String::from(COMPOSITOR_PLUGIN_PATH)],
            Vec::new(),
            compositor_authority(),
            OutputMode::Serial,
            exec_context.read_serial,
            exec_context.write_serial,
        ),
    );

    let turns = store.data().guest_turns();
    let wasm_instance = interrupts
        .drive(turns.drive(instance_pre.instantiate_async(&mut store)))
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
    let turns = store.data().guest_turns();
    interrupts
        .drive(turns.drive(store.run_concurrent(async move |accessor| {
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
        })))
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
    }
}

/// Tell the compositor about one client's window, and show the client's
/// frame buffer at the span the compositor answers with.
///
/// The compositor names the span, so it is told about the window before
/// the buffer is shown; the interface promises it no `commit` for the
/// window before the buffer is there, and this task serves the commit
/// queue, so none is served in between.
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
        buffer,
        reply,
    } = create;
    // A client that let the window go while this request was queued has
    // already been answered by its own side; the compositor is never
    // told about a window that is no longer on the desktop.
    if registry.lookup(id).is_none() {
        let _ = reply.send(Err(SurfaceServiceError::Gone));
        return Ok(());
    }

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
        )
        .await?;
    let span = match answer {
        Ok(span) => Span::new(span.offset, span.length),
        Err(error) => {
            let _ = reply.send(Err(from_guest_error(error)));
            return Ok(());
        }
    };

    let shown = accessor.with(|mut access| {
        // Checked again, in the same step as the show: the client may
        // have let the window go while the compositor was answering —
        // dropped it, died, or stopped waiting for this create — and the
        // drain's destroy for it may already have run. A view taken now
        // would outlive it.
        if registry.lookup(id).is_none() {
            return Err(SurfaceServiceError::Gone);
        }
        let linear_memory = access.get().linear_memory;
        let memory = guest_memory(linear_memory, &access)?;
        let data = access.get();
        data.device
            .surfaces_mut()
            .show_view(&mut data.views, memory, span, id, &buffer)
    });
    if let Err(SurfaceServiceError::Gone) = shown {
        // The window went back on the return queue, and the drain may
        // have served that destroy before the compositor had taken the
        // window on, which leaves the compositor holding it. So it is
        // told here, whatever the drain did: nothing was shown at its
        // span, and a destroy for a window it already let go does
        // nothing.
        guest.call_destroy(accessor, id.raw()).await?;
        let _ = reply.send(Err(SurfaceServiceError::Gone));
        return Ok(());
    }
    if let Err(error) = shown {
        // The compositor answered with a span the kernel cannot show the
        // window at. It took the window on, so it is told the window is
        // gone again — its span was never anything but its own memory —
        // and the client is told why.
        tracing::warn!(
            target: "helios_kernel::supervisor",
            surface = id.raw(),
            %error,
            "the compositor answered create with a span the window cannot be shown at"
        );
        guest.call_destroy(accessor, id.raw()).await?;
        let _ = reply.send(Err(error));
        return Ok(());
    }
    let _ = reply.send(Ok(()));
    Ok(())
}

/// End the compositor's view of one window and take it off the desktop.
///
/// The view goes first: the interface promises the compositor its span
/// is ordinary zeroed memory again before `destroy` is called, and the
/// buffer goes back to the client's pool with the view's reference.
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
    accessor
        .with(|mut access| {
            let data = access.get();
            data.device.surfaces_mut().end_view(id, &mut data.views)
        })
        .map_err(view_trap)?;
    guest.call_destroy(accessor, id.raw()).await?;
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
        GuestError::BadSpan => SurfaceServiceError::CompositorBadSpan,
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

/// The drain that takes back every window a client let go: the windows
/// of a client that died, a window a client dropped, and a window whose
/// `create` its client stopped waiting for.
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
        // completed wait: windows handed back between a drain and a park
        // are ones this task is owed.
        let mut waiter = registry.returns_waiter();
        loop {
            for returned in registry.take_returned() {
                // The view ends and the window goes off the desktop;
                // the client's buffer goes back to its pool with the
                // view's reference.
                destroy_surface(accessor, &self.guest, returned.id()).await?;
                // Only now does the window's slot come back. A failure
                // above ends this task and drops the guards it still
                // holds, which releases them the way the dead
                // compositor's windows are.
                returned.drained();
            }
            core::future::poll_fn(|cx| registry.poll_returns(cx, &mut waiter)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DesktopRefusal, compositor_authority, desktop_refusal};
    use crate::test_support::TestRuntimeState;

    /// The compositor draws the display and reads the input devices, and
    /// is given nothing else of the machine's hardware.
    #[test]
    fn provisioned_compositor_claims_the_display_and_every_input_device() {
        let authority = compositor_authority();

        assert!(authority.derive_display_claim_cap().is_ok());
        assert!(authority.derive_input_claim_cap().is_ok());
        assert!(authority.derive_gpu_open_cap().is_err());
        assert!(authority.derive_audio_claim_cap().is_err());
        assert!(authority.granted_devices().is_empty());
        assert!(
            authority.derive_spawn_authority().is_ok(),
            "it keeps what root grants: it spawns the shell it draws"
        );
    }

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
