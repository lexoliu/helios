//! The 3D path's own tests.
//!
//! The device side is a recording fake: every call the owner tasks make
//! is appended to a log, so a test asserts on the exact sequence the
//! display engine was asked for rather than on what the kernel
//! intended. The address space is the recording surface
//! `device::platform`'s test hooks install, so a test can also see when
//! a buffer's run was taken, shown and given back.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::AtomicBool;

use futures::channel::oneshot;
use futures_lite::future::{block_on, poll_once};
use helios_hal::device::{DeviceRegion, DeviceRegionAttributes};
use helios_hal::display::{
    BlobId, BlobMemory, BlobRequest, BlobUsage, CapsetId, CapsetInfo, CapsetList, ContextId,
    ContextName, FenceId, Gpu3d, Gpu3dError, Gpu3dResult,
};
use helios_hal::iommu::PhysicalRange;
use helios_hal::vmm::VirtAddr;
use std::sync::Mutex;
use triomphe::Arc;

use crate::buffer::{Buffer, GuestMemory, Span, ViewTable, WASM_PAGE_BYTES};
use crate::component::ProviderReceiver;
use crate::device::test_hooks;
use crate::display::SequenceSignal;
use crate::process::test_caps;
use crate::test_support::TestCpu;

use super::owner::{gpu3d_channels, serve_control, serve_submit};
use super::service::{Gpu3dRequest, Gpu3dService, Gpu3dShared, SubmitRequest};
use super::{Gpu3dOwnership, Gpu3dServiceError, MAX_GPU_VIEWS};

/// Where the tests pretend the claiming instance's memory sits, and how
/// far it has grown.
const MEMORY_BASE: usize = 0x1_0000_0000;
const MEMORY_BYTES: u64 = 64 * WASM_PAGE_BYTES;

fn memory() -> GuestMemory {
    GuestMemory::new(VirtAddr::new(MEMORY_BASE), MEMORY_BYTES)
}

/// `pages` wasm pages of the instance's memory starting at page `first`.
const fn pages(first: u64, pages: u64) -> Span {
    Span::new(first * WASM_PAGE_BYTES, pages * WASM_PAGE_BYTES)
}

/// A command buffer or a guest blob's storage, as the host functions pin
/// one.
fn pinned(bytes: u64) -> Buffer {
    Buffer::pinned(bytes).expect("the test pool has room")
}

/// The capset the fake renderer speaks.
const CAPSET: CapsetId = CapsetId::VENUS;

/// One thing the display engine's rendering half was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Capsets,
    Capset(CapsetId, u32),
    CreateContext(CapsetId),
    DestroyContext(ContextId),
    CreateBlob(ContextId, BlobMemory, u64),
    DestroyBlob(BlobId),
    MapBlob(BlobId),
    UnmapBlob(BlobId),
    Attach(ContextId, BlobId),
    Detach(ContextId, BlobId),
    Submit(ContextId, FenceId),
}

/// A display engine that records what its rendering half was asked and
/// always agrees.
struct FakeGpu3d {
    calls: Mutex<Vec<Call>>,
    /// The capability-set payload [`Gpu3d::capset`] writes back.
    capset_bytes: Vec<u8>,
    /// Set to make every submission fail, as a device refusing a
    /// command stream does.
    refuse_submits: AtomicBool,
    /// When `Some`, `submit` records the call and then parks on the
    /// receiver until the test lets it answer — a device still working
    /// on a chain it was given.
    submit_hold: Mutex<Option<oneshot::Receiver<()>>>,
    next_context: Mutex<u32>,
    next_blob: Mutex<u32>,
}

impl FakeGpu3d {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            capset_bytes: alloc::vec![0xde, 0xad, 0xbe, 0xef],
            refuse_submits: AtomicBool::new(false),
            submit_hold: Mutex::new(None),
            next_context: Mutex::new(1),
            next_blob: Mutex::new(1),
        }
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("no test panics here").push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("no test panics here").clone()
    }
}

impl Gpu3d for FakeGpu3d {
    fn renders(&self) -> bool {
        true
    }

    async fn capsets(&self) -> Gpu3dResult<CapsetList> {
        self.record(Call::Capsets);
        let mut list = CapsetList::new();
        list.push(CapsetInfo {
            id: CAPSET,
            max_version: 1,
            max_size: 4,
        });
        Ok(list)
    }

    async fn capset(&self, id: CapsetId, version: u32, out: &mut [u8]) -> Gpu3dResult<usize> {
        self.record(Call::Capset(id, version));
        let written = self.capset_bytes.len().min(out.len());
        out[..written].copy_from_slice(&self.capset_bytes[..written]);
        Ok(written)
    }

    async fn create_context(&self, capset: CapsetId, _name: ContextName) -> Gpu3dResult<ContextId> {
        let id = {
            let mut next = self.next_context.lock().expect("no test panics here");
            let id = ContextId::new(*next);
            *next += 1;
            id
        };
        self.record(Call::CreateContext(capset));
        Ok(id)
    }

    async fn destroy_context(&self, context: ContextId) -> Gpu3dResult<()> {
        self.record(Call::DestroyContext(context));
        Ok(())
    }

    async fn create_blob(&self, request: BlobRequest<'_>) -> Gpu3dResult<BlobId> {
        let id = {
            let mut next = self.next_blob.lock().expect("no test panics here");
            let id = BlobId::new(*next);
            *next += 1;
            id
        };
        self.record(Call::CreateBlob(
            request.context,
            request.memory,
            request.size,
        ));
        Ok(id)
    }

    async fn destroy_blob(&self, blob: BlobId) -> Gpu3dResult<()> {
        self.record(Call::DestroyBlob(blob));
        Ok(())
    }

    async fn map_blob(&self, blob: BlobId) -> Gpu3dResult<DeviceRegion> {
        self.record(Call::MapBlob(blob));
        Ok(DeviceRegion::new(
            PhysicalRange::new(0x8_0000_0000, 0x1000),
            DeviceRegionAttributes::PREFETCHABLE_MEMORY,
        ))
    }

    async fn unmap_blob(&self, blob: BlobId) -> Gpu3dResult<()> {
        self.record(Call::UnmapBlob(blob));
        Ok(())
    }

    async fn attach_resource(&self, context: ContextId, blob: BlobId) -> Gpu3dResult<()> {
        self.record(Call::Attach(context, blob));
        Ok(())
    }

    async fn detach_resource(&self, context: ContextId, blob: BlobId) -> Gpu3dResult<()> {
        self.record(Call::Detach(context, blob));
        Ok(())
    }

    async fn submit(
        &self,
        context: ContextId,
        commands: PhysicalRange,
        fence: FenceId,
    ) -> Gpu3dResult<()> {
        self.record(Call::Submit(context, fence));
        let hold = self.submit_hold.lock().expect("no test panics here").take();
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        if self
            .refuse_submits
            .load(core::sync::atomic::Ordering::Relaxed)
        {
            return Err(helios_hal::display::Gpu3dError::Unspecified);
        }
        assert!(commands.bytes > 0, "a command buffer covers bytes");
        Ok(())
    }

    async fn fences(&self, _context: ContextId, after: FenceId) -> Gpu3dResult<FenceId> {
        // The kernel never asks the device for a fence: the submit
        // server's completion is the signal. Resolving with the point
        // asked after keeps a stray caller honest without parking it.
        Ok(after)
    }
}

/// Run `work` with both servers running beside it.
///
/// The servers never return, so the result is the work's; what they are
/// there for is to answer the requests the work makes.
fn with_servers<T>(
    device: &FakeGpu3d,
    shared: &Gpu3dShared,
    control: &ProviderReceiver<Gpu3dRequest>,
    submit: &ProviderReceiver<SubmitRequest>,
    work: impl Future<Output = T>,
) -> T {
    let cpu = TestCpu::without_entropy();
    block_on(async {
        let work = pin!(work);
        let servers = pin!(async {
            futures::future::join(
                serve_control(device, shared, control),
                serve_submit(device, shared, submit, &cpu),
            )
            .await;
        });
        futures_lite::future::or(async { Some(work.await) }, async {
            servers.await;
            None
        })
        .await
        .expect("the 3D servers do not end on their own")
    })
}

/// A claim whose views go in a set of `views`.
fn claimed_in(shared: &Arc<Gpu3dShared>, views: &mut ViewTable) -> Gpu3dOwnership {
    let service = Gpu3dService::from_shared(shared.clone());
    let mut ownership = Gpu3dOwnership::new();
    ownership
        .claim(&service, views.group(), test_caps::gpu())
        .expect("the engine is free");
    ownership
}

/// A claim for a test that shows nothing.
fn claimed(shared: &Arc<Gpu3dShared>) -> Gpu3dOwnership {
    claimed_in(shared, &mut ViewTable::new())
}

/// Ask the control server to open one context for `ownership`.
async fn create_context(ownership: &Gpu3dOwnership) -> super::ContextRecord {
    let claim = ownership.claim_ref().expect("the claim is held");
    let (reply, answer) = oneshot::channel();
    claim
        .sender()
        .control(
            Gpu3dRequest::CreateContext {
                generation: claim.generation(),
                capset: CAPSET,
                name: ContextName::new(),
                reply,
            },
            answer,
        )
        .await
        .expect("the fake engine agrees")
}

/// An instance is refused the engine another instance holds, and is
/// still refused while the first owner's resources are being handed
/// back: the engine is not free until the display engine says it is.
#[test]
fn a_second_instance_is_refused_the_engine_the_first_holds() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let service = Gpu3dService::from_shared(shared.clone());

    let first = service.claim(test_caps::gpu()).expect("the engine is free");
    assert_eq!(
        service.claim(test_caps::gpu()).err(),
        Some(Gpu3dServiceError::AlreadyClaimed)
    );

    drop(first);
    assert_eq!(
        service.claim(test_caps::gpu()).err(),
        Some(Gpu3dServiceError::AlreadyClaimed)
    );
}

/// A device that renders nothing still answers the claim — with
/// `no-renderer` rather than `unavailable`, because the machine has a
/// display engine; it simply has no renderer on it.
#[test]
fn an_engine_that_renders_nothing_cannot_be_claimed() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(false);
    let service = Gpu3dService::from_shared(shared);

    assert_eq!(
        service.claim(test_caps::gpu()).err(),
        Some(Gpu3dServiceError::NoRenderer)
    );
    assert!(!service.is_claimed());
}

/// One instance holds one engine: a second claim from the same store
/// would make the release ambiguous.
#[test]
fn one_instance_holds_the_engine_once() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let service = Gpu3dService::from_shared(shared.clone());
    let mut ownership = Gpu3dOwnership::new();

    let group = ViewTable::new().group();
    ownership
        .claim(&service, group, test_caps::gpu())
        .expect("the engine is free");
    assert_eq!(
        ownership.claim(&service, group, test_caps::gpu()).err(),
        Some(Gpu3dServiceError::AlreadyClaimed)
    );
}

/// Giving the engine back ends every view the claim showed — command
/// buffers, guest blobs and aperture placements alike — and a run shown
/// nowhere and held by nothing else goes back to the pool with it.
#[test]
fn releasing_the_claim_ends_every_view_in_its_set() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let mut views = ViewTable::new();
    let mut ownership = claimed_in(&shared, &mut views);
    let group = ownership.views().expect("a claim has a set");
    let runs = test_hooks::live_runs();

    let commands = pinned(4096);
    let physical = commands.physical();
    views
        .show_buffer(memory(), pages(0, 1), &commands, Some(group))
        .expect("the span is free");
    drop(commands);
    views
        .show_device(
            memory(),
            pages(1, 1),
            DeviceRegion::new(
                PhysicalRange::new(0x8_0000_0000, 0x1000),
                DeviceRegionAttributes::PREFETCHABLE_MEMORY,
            ),
            Some(group),
        )
        .expect("the span is free");
    let before = test_hooks::changes().len();

    ownership
        .release(&mut views)
        .expect("the test pool refills anything");

    assert!(views.is_empty(), "nothing of the claim is shown");
    assert!(!ownership.holds_gpu());
    let changes = &test_hooks::changes()[before..];
    assert_eq!(
        changes
            .iter()
            .filter(|change| matches!(change, test_hooks::MappingChange::Restore(_)))
            .count(),
        2,
        "both spans are the instance's ordinary memory again"
    );
    assert!(changes.contains(&test_hooks::MappingChange::Freed(physical)));
    assert_eq!(test_hooks::live_runs(), runs);
}

/// An aperture placement is shown as the device's own bytes: nothing is
/// taken from the pool for it, because the memory behind it is the
/// renderer's.
#[test]
fn a_mapped_blob_is_a_device_region_not_a_run() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let mut views = ViewTable::new();
    let ownership = claimed_in(&shared, &mut views);
    let before = test_hooks::changes().len();

    views
        .show_device(
            memory(),
            pages(2, 1),
            DeviceRegion::new(
                PhysicalRange::new(0x8_0000_0000, 0x1000),
                DeviceRegionAttributes::PREFETCHABLE_MEMORY,
            ),
            Some(ownership.views().expect("a claim has a set")),
        )
        .expect("the span is free");

    assert_eq!(
        &test_hooks::changes()[before..],
        &[test_hooks::MappingChange::SubstituteDevice(
            helios_hal::vmm::VirtRange::new(
                VirtAddr::new(MEMORY_BASE + 2 * WASM_PAGE_BYTES as usize),
                0x1000
            )
        )],
        "the aperture is shown like a device's own frames"
    );
}

/// A claim shows at most its share of the store's views: the next one is
/// refused before anything is substituted, and ending one makes room.
#[test]
fn a_claim_shows_at_most_its_share_of_views() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let mut views = ViewTable::new();
    let ownership = claimed_in(&shared, &mut views);
    let region = DeviceRegion::new(
        PhysicalRange::new(0x8_0000_0000, 0x1000),
        DeviceRegionAttributes::PREFETCHABLE_MEMORY,
    );
    let mut shown = Vec::new();
    for page in 0..MAX_GPU_VIEWS as u64 {
        let group = ownership
            .room_for_view(&views)
            .expect("the claim is under its share");
        shown.push(
            views
                .show_device(memory(), pages(page, 1), region, Some(group))
                .expect("the span is free"),
        );
    }

    assert_eq!(
        ownership.room_for_view(&views).err(),
        Some(Gpu3dServiceError::TooManyViews)
    );
    views
        .end(shown.pop().expect("the claim shows something"))
        .expect("the test pool refills anything");
    assert!(ownership.room_for_view(&views).is_ok());
}

/// The context type is checked against the device's own capability
/// sets: an engine that negotiated no context-init would silently hand
/// a venus asker a virgl context, and the guest could not tell.
#[test]
fn a_context_is_refused_for_a_renderer_the_host_does_not_speak() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = claimed(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let claim = ownership.claim_ref().expect("the claim is held");
        let (reply, answer) = oneshot::channel();
        let refused = claim
            .sender()
            .control(
                Gpu3dRequest::CreateContext {
                    generation: claim.generation(),
                    capset: CapsetId::GFXSTREAM_VULKAN,
                    name: ContextName::new(),
                    reply,
                },
                answer,
            )
            .await;
        assert_eq!(refused.err(), Some(Gpu3dServiceError::UnsupportedContext));
    });

    assert_eq!(
        device.calls(),
        alloc::vec![Call::Capsets],
        "the context never reached the device"
    );
}

/// A capability set is the renderer's bytes: read once for the list,
/// then fetched whole for the guest that asked, and never interpreted.
#[test]
fn a_capset_s_bytes_reach_their_reader_undecoded() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = claimed(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let claim = ownership.claim_ref().expect("the claim is held");
        let (reply, answer) = oneshot::channel();
        let bytes = claim
            .sender()
            .control(
                Gpu3dRequest::Capset {
                    generation: claim.generation(),
                    id: CAPSET,
                    reply,
                },
                answer,
            )
            .await
            .expect("the fake engine agrees");
        assert_eq!(bytes, alloc::vec![0xde, 0xad, 0xbe, 0xef]);

        let (reply, answer) = oneshot::channel();
        let refused = claim
            .sender()
            .control(
                Gpu3dRequest::Capset {
                    generation: claim.generation(),
                    id: CapsetId::VIRGL,
                    reply,
                },
                answer,
            )
            .await;
        assert_eq!(refused.err(), Some(Gpu3dServiceError::NoSuchCapset));
    });
}

/// A blob names the context whose renderer owns it; a context the claim
/// never opened is not a place a resource can live.
#[test]
fn a_blob_for_a_context_the_claim_never_opened_is_refused() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = claimed(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let claim = ownership.claim_ref().expect("the claim is held");
        let (reply, answer) = oneshot::channel();
        let refused = claim
            .sender()
            .control(
                Gpu3dRequest::CreateBlob {
                    generation: claim.generation(),
                    spec: crate::gpu::BlobSpec {
                        context: ContextId::new(77),
                        memory: BlobMemory::Guest,
                        usage: BlobUsage::empty(),
                        size: 4096,
                        host_id: 0,
                        backing: Some(pinned(4096)),
                    },
                    reply,
                },
                answer,
            )
            .await;
        assert_eq!(refused.err(), Some(Gpu3dServiceError::InvalidBlob));
    });

    assert!(
        !device
            .calls()
            .iter()
            .any(|call| matches!(call, Call::CreateBlob(..))),
        "the blob never reached the device"
    );
}

/// A submission's completion is its fence: the context's signal carries
/// it whether the device agreed with the command stream or not.
#[test]
fn a_submission_publishes_its_fence() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = claimed(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let commands = pinned(4096);
        let claim = ownership.claim_ref().expect("the claim is held");

        let (reply, answer) = oneshot::channel();
        claim
            .sender()
            .submit(
                SubmitRequest::Submit {
                    generation: claim.generation(),
                    context: context.id,
                    commands: commands.physical(),
                    buffer: commands,
                    fence: FenceId::new(7),
                    fences: context.fences.clone(),
                    reply,
                },
                answer,
            )
            .await
            .expect("the fake engine agrees");
        assert_eq!(
            context.fences.sequence(),
            7,
            "the fence stream sees the fence the submission carried"
        );
    });

    assert!(
        device
            .calls()
            .contains(&Call::Submit(ContextId::new(1), FenceId::new(7))),
        "the device saw the submission"
    );
}

/// A command the device refuses still retires its fence: a reader of
/// the context's stream must not wait for a point the device decided
/// would not come.
#[test]
fn a_rejected_submission_still_retires_its_fence() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    device
        .refuse_submits
        .store(true, core::sync::atomic::Ordering::Relaxed);
    let ownership = claimed(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let commands = pinned(4096);
        let claim = ownership.claim_ref().expect("the claim is held");

        let (reply, answer) = oneshot::channel();
        let refused = claim
            .sender()
            .submit(
                SubmitRequest::Submit {
                    generation: claim.generation(),
                    context: context.id,
                    commands: commands.physical(),
                    buffer: commands,
                    fence: FenceId::new(3),
                    fences: context.fences.clone(),
                    reply,
                },
                answer,
            )
            .await;
        assert!(refused.is_err(), "the device refused the command");
        assert_eq!(context.fences.sequence(), 3, "its fence retired anyway");
    });
}

/// Letting go gives the aperture placements back before the blobs, and
/// the blobs before the contexts: a renderer still holding a context
/// that names dead resources would be reading guest pages on their way
/// back to a pool. The blob's buffer goes back only after the device
/// has destroyed the blob, because the owner task's record holds it.
#[test]
fn a_released_claim_unmaps_then_destroys_then_frees() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut views = ViewTable::new();
    let mut ownership = claimed_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let claim = ownership.claim_ref().expect("the claim is held");
        let generation = claim.generation();
        let sender = claim.sender();

        let (reply, answer) = oneshot::channel();
        let blob = sender
            .control(
                Gpu3dRequest::CreateBlob {
                    generation,
                    spec: crate::gpu::BlobSpec {
                        context: context.id,
                        memory: BlobMemory::Host3dGuest,
                        usage: BlobUsage::MAPPABLE,
                        size: 4096,
                        host_id: 0,
                        backing: Some(pinned(4096)),
                    },
                    reply,
                },
                answer,
            )
            .await
            .expect("the fake engine agrees");

        let (reply, answer) = oneshot::channel();
        sender
            .control(
                Gpu3dRequest::MapBlob {
                    generation,
                    blob,
                    reply,
                },
                answer,
            )
            .await
            .expect("the fake engine agrees");

        assert_eq!(test_hooks::live_runs() - runs, 1);

        // Giving the engine back while alive is this; dying is the same
        // with the views withdrawn by the store's table first.
        ownership
            .release(&mut views)
            .expect("the test pool refills anything");
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the blob's buffer is not freed by the release itself"
        );

        crate::yield_now().await;
        crate::yield_now().await;
    });

    let calls = device.calls();
    let unmap = calls
        .iter()
        .position(|call| matches!(call, Call::UnmapBlob(_)))
        .expect("the blob leaves the aperture");
    let destroy_blob = calls
        .iter()
        .position(|call| matches!(call, Call::DestroyBlob(_)))
        .expect("the blob is destroyed");
    let destroy_context = calls
        .iter()
        .position(|call| matches!(call, Call::DestroyContext(_)))
        .expect("the context is destroyed");
    assert!(
        unmap < destroy_blob && destroy_blob < destroy_context,
        "the renderer stops placing the storage before it forgets the resource"
    );
    assert!(
        matches!(
            test_hooks::changes().last(),
            Some(test_hooks::MappingChange::Freed(_))
        ),
        "the blob's buffer goes back once the renderer has let go"
    );
    assert_eq!(test_hooks::live_runs(), runs);

    let service = Gpu3dService::from_shared(shared.clone());
    assert!(
        service.claim(test_caps::gpu()).is_ok(),
        "the engine is free once its resources are back"
    );
}

/// A request made under a claim that has ended is dropped rather than
/// served: a dead plugin must not drive a live one's renderer.
#[test]
fn a_request_that_outlived_its_claim_is_not_served() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut ownership = claimed(&shared);
    let stale = ownership.claim_ref().expect("the claim is held").sender();
    let stale_generation = stale.generation();

    with_servers(&device, &shared, &control, &submit, async {
        // The first claim ends and a second one takes the engine.
        ownership
            .release(&mut ViewTable::new())
            .expect("the claim showed nothing");
        crate::yield_now().await;
        crate::yield_now().await;
        let _second = claimed(&shared);

        let (reply, answer) = oneshot::channel();
        let refused = stale
            .control(
                Gpu3dRequest::Capsets {
                    generation: stale_generation,
                    reply,
                },
                answer,
            )
            .await;
        assert_eq!(refused.err(), Some(Gpu3dServiceError::Closed));

        let (reply, answer) = oneshot::channel();
        let refused = stale
            .submit(
                SubmitRequest::Submit {
                    generation: stale_generation,
                    context: ContextId::new(1),
                    buffer: pinned(4096),
                    commands: PhysicalRange::new(0x4000_0000, 4096),
                    fence: FenceId::new(1),
                    fences: Arc::new(SequenceSignal::new()),
                    reply,
                },
                answer,
            )
            .await;
        assert_eq!(refused.err(), Some(Gpu3dServiceError::Closed));
    });

    assert!(
        device.calls().is_empty(),
        "nothing reached the device under a claim that had ended"
    );
}

/// A request sitting in the inbox when its claim is let go is dropped
/// rather than served, however long the engine then sits unclaimed:
/// the generation that delimits a claim's window advances on release,
/// not only on the next claim.
#[test]
fn a_request_queued_before_release_is_dropped() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut ownership = claimed(&shared);
    let claim = ownership.claim_ref().expect("the claim is held");
    let generation = claim.generation();
    let sender = claim.sender();

    // The request lands in the inbox before the servers run and before
    // the claim is let go: it is the one thing the owner task finds
    // waiting when it wakes for the release.
    let (reply, answer) = oneshot::channel();
    let mut pending = pin!(sender.control(
        Gpu3dRequest::CreateContext {
            generation,
            capset: CAPSET,
            name: ContextName::new(),
            reply,
        },
        answer,
    ));
    assert!(
        block_on(poll_once(pending.as_mut())).is_none(),
        "the request is queued and unanswered"
    );
    ownership
        .release(&mut ViewTable::new())
        .expect("the claim showed nothing");

    with_servers(&device, &shared, &control, &submit, async {
        assert_eq!(
            pending.await.err(),
            Some(Gpu3dServiceError::Closed),
            "the dead claim's request is dropped, not served"
        );
        crate::yield_now().await;
        crate::yield_now().await;
    });

    assert!(
        device.calls().is_empty(),
        "no device-side context was created for a claim that had ended"
    );
    let service = Gpu3dService::from_shared(shared);
    assert!(
        service.claim(test_caps::gpu()).is_ok(),
        "the engine is free once its resources are back"
    );
}

/// A submission the device is still working on is not torn down under
/// it: the release waits the in-flight tally out, so the destroy lands
/// on the control ring only after the device has finished the chain,
/// and the command buffer is not freed while the engine may still be
/// reading it — even once the instance's own view of it has ended.
#[test]
fn a_submission_in_flight_is_finished_before_the_claim_is_released() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let (gate, hold) = oneshot::channel();
    *device.submit_hold.lock().expect("no test panics here") = Some(hold);
    let service = Gpu3dService::from_shared(shared.clone());
    let mut views = ViewTable::new();
    let mut ownership = claimed_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let commands = pinned(4096);
        views
            .show_buffer(
                memory(),
                pages(0, 1),
                &commands,
                Some(ownership.views().expect("a claim has a set")),
            )
            .expect("the span is free");
        let generation = ownership
            .claim_ref()
            .expect("the claim is held")
            .generation();

        // The device takes the submission and keeps working on it, as
        // a chain parked for ring room is. The request's reference is
        // the only one left once the view ends.
        let (reply, answer) = oneshot::channel();
        shared
            .submit
            .send(SubmitRequest::Submit {
                generation,
                context: context.id,
                commands: commands.physical(),
                buffer: commands,
                fence: FenceId::new(1),
                fences: context.fences.clone(),
                reply,
            })
            .await
            .expect("the queue has room");
        for _ in 0..64 {
            if device
                .calls()
                .contains(&Call::Submit(context.id, FenceId::new(1)))
            {
                break;
            }
            crate::yield_now().await;
        }
        assert!(
            device
                .calls()
                .contains(&Call::Submit(context.id, FenceId::new(1))),
            "the device is holding the chain"
        );

        // The claim ends while the device holds it, and with it the
        // instance's view of the command buffer. The teardown is given
        // every chance to run ahead of the device.
        ownership
            .release(&mut views)
            .expect("the test pool refills anything");
        assert!(views.is_empty(), "the claim's views end with it");
        for _ in 0..8 {
            crate::yield_now().await;
        }
        assert!(
            !device
                .calls()
                .iter()
                .any(|call| matches!(call, Call::DestroyContext(_))),
            "the destroy is not issued while a submission is in flight"
        );
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the command buffer is not freed while the device holds the chain"
        );

        // The device finishes; only then may the claim's teardown run.
        gate.send(()).expect("the device is parked on it");
        answer
            .await
            .expect("the submission is answered")
            .expect("the fake engine agrees");
        while service.is_claimed() {
            crate::yield_now().await;
        }
    });

    let calls = device.calls();
    let submitted = calls
        .iter()
        .position(|call| matches!(call, Call::Submit(..)))
        .expect("the device saw the submission");
    let destroyed = calls
        .iter()
        .position(|call| matches!(call, Call::DestroyContext(_)))
        .expect("the context is destroyed once the device is done");
    assert!(
        submitted < destroyed,
        "the chain is taken before its context goes"
    );
    assert_eq!(
        test_hooks::live_runs(),
        runs,
        "the command buffer goes back once the device has answered"
    );
}

/// A submission too long for the wire's command-buffer field is a
/// bounds refusal, which is what the WIT's `out-of-bounds` is written
/// for — not `invalid-blob`, which is a blob's own parameters being
/// wrong.
#[test]
fn a_command_buffer_too_long_for_the_wire_is_out_of_bounds() {
    assert_eq!(
        Gpu3dServiceError::from(Gpu3dError::CommandBufferLength { bytes: 8 << 20 }),
        Gpu3dServiceError::OutOfBounds
    );
}

/// The guest-facing handles a component holds are the store's resource
/// table entries, so the stale-handle test builds a store for real: a
/// `command-buffer` or `blob` handle minted under one claim names that
/// claim's resources, and answered under the next it would be a run
/// somebody else owns. The fixture itself lives with the adapter,
/// because the store type names pieces that are adapter-only.
#[cfg(feature = "wasmtime-runtime")]
mod component_host {
    use wasmtime::component::Resource;

    use super::*;
    use crate::wasmtime_adapter::bindings::gpu::bindings::helios::system::gpu as contract;
    use crate::wasmtime_adapter::component_host::service::test_store;
    use crate::wasmtime_adapter::component_host::service::{BlobHandle, CommandBufferHandle};

    /// Show a fresh 4 KiB buffer at wasm page `page` of the store's
    /// memory, in the live claim's set.
    fn shown(data: &mut test_store::TestStoreData, page: u64) -> (Buffer, crate::ViewId) {
        let group = data.device.gpu().views().expect("the claim is held");
        let buffer = pinned(4096);
        let view = data
            .views
            .show_buffer(memory(), pages(page, 1), &buffer, Some(group))
            .expect("the span is free");
        (buffer, view)
    }

    /// A buffer a component kept across its claim's release was shown in
    /// a set the release already ended, so `submit` refuses it with the
    /// contract's word for a dead claim, and dropping it — or a blob of
    /// the same claim — ends nothing twice. A live claim's handle ends
    /// its own view when it is dropped.
    #[test]
    fn a_command_buffer_that_outlived_its_claim_is_refused() {
        test_hooks::install();
        let (shared, control, submit) = gpu3d_channels(true);
        let device = FakeGpu3d::new();
        let service = Gpu3dService::from_shared(shared.clone());
        let mut data = test_store::store_data(&service);
        data.device
            .claim_gpu(&service, &mut data.views, test_caps::gpu())
            .expect("the engine is free");
        let first = data.device.gpu().claim_ref().expect("held").generation();
        let (buffer, view) = shown(&mut data, 0);
        let commands = data
            .table
            .push(CommandBufferHandle {
                generation: first,
                buffer,
                view,
            })
            .expect("the table has room");
        let (_, backing) = shown(&mut data, 1);
        let blob = data
            .table
            .push(BlobHandle {
                generation: first,
                id: BlobId::new(9),
                backing: Some(backing),
                mapped: None,
            })
            .expect("the table has room");

        // The claim ends, and with it every view it showed; the handles
        // live on, as they do in a store nobody told. Whoever claims the
        // engine next holds it under a generation that is not theirs.
        with_servers(&device, &shared, &control, &submit, async {
            data.device
                .gpu_mut()
                .release(&mut data.views)
                .expect("the test pool refills anything");
            while service.is_claimed() {
                crate::yield_now().await;
            }
            data.device
                .claim_gpu(&service, &mut data.views, test_caps::gpu())
                .expect("the engine is free once its resources are back");
        });
        assert!(data.views.is_empty(), "the release ended the claim's views");
        let live = data.device.gpu().claim_ref().expect("held").generation();
        assert_ne!(live, first, "a release and a claim both move it");

        let refused = data
            .command_range(&commands, 0, 16, live)
            .expect("the handle is still in the table");
        assert_eq!(
            refused.err(),
            Some(contract::Error::NotClaimed),
            "submit's run check answers not-claimed for a dead claim's buffer"
        );
        assert!(
            contract::HostCommandBuffer::drop(&mut data, commands).is_ok(),
            "a dead claim's buffer ends no view a second time"
        );
        assert!(
            contract::HostBlob::drop(&mut data, blob).is_ok(),
            "a dead claim's blob ends no view a second time"
        );

        let (buffer, view) = shown(&mut data, 2);
        let commands = data
            .table
            .push(CommandBufferHandle {
                generation: live,
                buffer,
                view,
            })
            .expect("the table has room");
        contract::HostCommandBuffer::drop(&mut data, commands)
            .expect("a live claim's buffer ends its view");
        assert!(
            !data.views.is_shown(view),
            "the span is ordinary memory again"
        );
    }

    /// A context that outlived its claim must not hand out the dead
    /// claim's fence signal — nothing publishes to it again, so the
    /// guest's stream read would hang — so `fences` traps: its signature
    /// cannot carry `not-claimed`.
    #[test]
    fn a_context_that_outlived_its_claim_opens_no_fence_stream() {
        test_hooks::install();
        let (shared, control, submit) = gpu3d_channels(true);
        let device = FakeGpu3d::new();
        let service = Gpu3dService::from_shared(shared.clone());
        let mut data = test_store::store_data(&service);
        data.device
            .claim_gpu(&service, &mut data.views, test_caps::gpu())
            .expect("the engine is free");
        let first = data.device.gpu().claim_ref().expect("held").generation();
        let context = data
            .table
            .push(test_store::context_handle(first, ContextId::new(4)))
            .expect("the table has room");

        // The claim ends and another takes the engine; the handle lives
        // on, as it does in a store nobody told.
        with_servers(&device, &shared, &control, &submit, async {
            data.device
                .gpu_mut()
                .release(&mut data.views)
                .expect("the claim showed nothing");
            while service.is_claimed() {
                crate::yield_now().await;
            }
            data.device
                .claim_gpu(&service, &mut data.views, test_caps::gpu())
                .expect("the engine is free once its resources are back");
        });
        let live = data.device.gpu().claim_ref().expect("held").generation();
        assert_ne!(live, first, "a release and a claim both move it");
        let live_context = data
            .table
            .push(test_store::context_handle(live, ContextId::new(5)))
            .expect("the table has room");

        // `fences` is a with-store host call, so it wants a real store
        // to take its `Access` from.
        let engine =
            wasmtime::Engine::new(&wasmtime::Config::new()).expect("the host engine builds");
        let mut store = wasmtime::Store::new(&engine, data);
        type GpuAccess<'a> = wasmtime::component::Access<
            'a,
            test_store::TestStoreData,
            wasmtime::component::HasSelf<test_store::TestStoreData>,
        >;
        use wasmtime::AsContextMut;
        let refused = contract::HostContextWithStore::fences(
            GpuAccess::new(store.as_context_mut(), |data| data),
            Resource::new_borrow(context.rep()),
        );
        assert!(
            refused.is_err_and(
                |error| alloc::format!("{error}").contains("does not hold the 3D engine")
            ),
            "the dead claim's context is a trap, not a stream nobody feeds"
        );
        let served = contract::HostContextWithStore::fences(
            GpuAccess::new(store.as_context_mut(), |data| data),
            Resource::new_borrow(live_context.rep()),
        );
        assert!(
            served.is_ok(),
            "the live claim's context still opens its stream"
        );
    }

    fn authority_with(classes: crate::DeviceClassRights) -> crate::ProcessAuthority {
        let mut authority = crate::ProcessAuthority::empty();
        authority.grant_device_classes(classes);
        authority
    }

    /// An ordinary program is told what a machine with no 3D service
    /// tells everyone, not `no-renderer`, and the engine stays free for
    /// its owner.
    #[test]
    fn ordinary_program_is_refused_the_gpu_as_if_there_were_none() {
        test_hooks::install();
        let (shared, _control, _submit) = gpu3d_channels(true);
        let service = Gpu3dService::from_shared(shared.clone());

        for authority in [
            crate::ProcessAuthority::empty(),
            crate::ProcessAuthority::root(),
            authority_with(crate::DeviceClassRights::all() - crate::DeviceClassRights::GPU),
        ] {
            let mut data = test_store::store_data_authorized(authority, |runtime_state| {
                runtime_state.install_gpu3d_service(service.clone());
            });
            let answer = <test_store::TestStoreData as contract::Host>::claim(&mut data)
                .expect("a refusal is an answer, not a trap");
            assert_eq!(answer.err(), Some(contract::Error::Unavailable));
            assert!(!data.device.gpu().holds_gpu());
        }
        assert!(!service.is_claimed(), "nobody took the engine");

        let mut absent =
            test_store::store_data_authorized(crate::ProcessAuthority::empty(), |_| {});
        let none = <test_store::TestStoreData as contract::Host>::claim(&mut absent)
            .expect("a machine with no 3D service answers");
        assert_eq!(
            none.err(),
            Some(contract::Error::Unavailable),
            "the answer is the one a machine with no 3D service gives"
        );
    }

    /// A program that holds the GPU class claims the engine.
    #[test]
    fn debugger_granted_child_claims_the_gpu() {
        test_hooks::install();
        let (shared, _control, _submit) = gpu3d_channels(true);
        let service = Gpu3dService::from_shared(shared.clone());
        let mut data = test_store::store_data_authorized(
            authority_with(crate::DeviceClassRights::GPU),
            |runtime_state| {
                runtime_state.install_gpu3d_service(service.clone());
            },
        );

        let answer = <test_store::TestStoreData as contract::Host>::claim(&mut data)
            .expect("the claim is answered");

        assert!(answer.is_ok(), "the GPU class opens the engine");
        assert!(service.is_claimed());
    }
}
