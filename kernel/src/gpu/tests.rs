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
use core::sync::atomic::{AtomicBool, Ordering};

use futures::channel::oneshot;
use futures_lite::future::{block_on, poll_once};
use helios_hal::device::{DeviceRegion, DeviceRegionAttributes};
use helios_hal::display::{
    BlobId, BlobMemory, BlobRequest, BlobUsage, CapsetId, CapsetInfo, CapsetList, CommandStream,
    ContextId, ContextName, FenceId, Gpu3d, Gpu3dError, Gpu3dResult, RingCount, RingIndex,
    Submission,
};
use helios_hal::io::IoError;
use helios_hal::iommu::PhysicalRange;
use helios_hal::vmm::VirtAddr;
use std::sync::Mutex;
use triomphe::Arc;

use crate::buffer::{Buffer, GuestMemory, Span, ViewTable, WASM_PAGE_BYTES};
use crate::component::ProviderReceiver;
use crate::device::test_hooks;
use crate::process::test_caps;
use crate::test_support::TestCpu;

use super::owner::{gpu3d_channels, serve_control, serve_submit};
use super::service::{
    BlobShared, BlobSpec, ContextShared, ControlMessage, Gpu3dRequest, Gpu3dSender, Gpu3dService,
    Gpu3dShared, InlineCommands, ScanoutHold, SubmitCommands, SubmitMessage, SubmitRequest,
};
use super::{
    ContextRecord, Gpu3dOwnership, Gpu3dServiceError, MAX_GPU_BLOBS, MAX_GPU_CONTEXTS,
    MAX_GPU_VIEWS, MAX_MACHINE_GPU_BLOBS, MAX_MACHINE_GPU_CONTEXTS,
};

/// Where the tests pretend the instance's memory sits, and how far it
/// has grown.
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

/// The newest version of [`CAPSET`] the fake renderer publishes.
const CAPSET_MAX_VERSION: u32 = 1;

/// Where the fake engine's aperture placements land: an address above
/// any the test pool hands out, so a placement is never mistaken for a
/// run of it.
const APERTURE: u64 = 0x8_0000_0000;

/// One thing the display engine's rendering half was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Capsets,
    Capset(CapsetId, u32),
    CreateContext(CapsetId, u8),
    DestroyContext(ContextId),
    CreateBlob(ContextId, BlobMemory, u64, u64),
    DestroyBlob(BlobId),
    MapBlob(BlobId),
    UnmapBlob(BlobId),
    Attach(ContextId, BlobId),
    Detach(ContextId, BlobId),
    Submit(ContextId, u8, Option<FenceId>),
}

/// A display engine that records what its rendering half was asked and
/// agrees, unless a test made it refuse something.
struct FakeGpu3d {
    calls: Mutex<Vec<Call>>,
    /// The capability-set payload [`Gpu3d::capset`] writes back.
    capset_bytes: Vec<u8>,
    /// Submissions for this context are refused by the device after
    /// they reached its ring, as a renderer that rejects a command
    /// stream answers.
    refuse_context: Mutex<Option<ContextId>>,
    /// Set to make every submission fail before it reaches the ring, as
    /// a driver that cannot post the chain does.
    refuse_before_post: AtomicBool,
    /// When `Some`, `submit` posts the chain and then parks on the
    /// receiver until the test lets it answer — a device still working
    /// on a chain it was given.
    submit_hold: Mutex<Option<oneshot::Receiver<()>>>,
    /// Set to make every submission end in a transport error, as a
    /// faulted queue does: the chain is left published and nothing
    /// answers it.
    transport_fault_submits: AtomicBool,
    /// Like `submit_hold`, for `destroy_blob`.
    destroy_hold: Mutex<Option<oneshot::Receiver<()>>>,
    /// Set to make every blob destroy fail.
    refuse_destroy: AtomicBool,
    /// Set to make every blob unmap fail.
    refuse_unmap: AtomicBool,
    /// Set to make every blob create fail: an answered refusal when
    /// false is stored with it, a transport error when true.
    fail_creates: Mutex<Option<bool>>,
    /// The memory type every placement is reported under.
    map_attributes: DeviceRegionAttributes,
    next_context: Mutex<u32>,
    next_blob: Mutex<u32>,
}

impl FakeGpu3d {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            capset_bytes: alloc::vec![0xde, 0xad, 0xbe, 0xef],
            refuse_context: Mutex::new(None),
            refuse_before_post: AtomicBool::new(false),
            submit_hold: Mutex::new(None),
            transport_fault_submits: AtomicBool::new(false),
            destroy_hold: Mutex::new(None),
            refuse_destroy: AtomicBool::new(false),
            refuse_unmap: AtomicBool::new(false),
            fail_creates: Mutex::new(None),
            map_attributes: DeviceRegionAttributes::PREFETCHABLE_MEMORY,
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

    fn saw(&self, call: &Call) -> bool {
        self.calls().contains(call)
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
            max_version: CAPSET_MAX_VERSION,
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

    async fn create_context(
        &self,
        capset: CapsetId,
        rings: RingCount,
        _name: ContextName,
    ) -> Gpu3dResult<ContextId> {
        let id = {
            let mut next = self.next_context.lock().expect("no test panics here");
            let id = ContextId::new(*next);
            *next += 1;
            id
        };
        self.record(Call::CreateContext(capset, rings.get()));
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
            request.blob_id,
        ));
        match *self.fail_creates.lock().expect("no test panics here") {
            Some(true) => Err(Gpu3dError::Transport(IoError::DeviceFault)),
            Some(false) => Err(Gpu3dError::Unspecified),
            None => Ok(id),
        }
    }

    async fn destroy_blob(&self, blob: BlobId) -> Gpu3dResult<()> {
        self.record(Call::DestroyBlob(blob));
        let hold = self
            .destroy_hold
            .lock()
            .expect("no test panics here")
            .take();
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        if self.refuse_destroy.load(Ordering::Relaxed) {
            return Err(Gpu3dError::Unspecified);
        }
        Ok(())
    }

    async fn map_blob(&self, blob: BlobId) -> Gpu3dResult<DeviceRegion> {
        self.record(Call::MapBlob(blob));
        Ok(DeviceRegion::new(
            PhysicalRange::new(APERTURE, 0x1000),
            self.map_attributes,
        ))
    }

    async fn unmap_blob(&self, blob: BlobId) -> Gpu3dResult<()> {
        self.record(Call::UnmapBlob(blob));
        if self.refuse_unmap.load(Ordering::Relaxed) {
            return Err(Gpu3dError::Unspecified);
        }
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

    async fn submit<'a, Posted>(
        &'a self,
        submission: Submission<'a>,
        posted: &'a mut Posted,
    ) -> Gpu3dResult<()>
    where
        Posted: FnMut() + Send,
    {
        self.record(Call::Submit(
            submission.context,
            submission.ring.raw(),
            submission.fence,
        ));
        match submission.commands {
            CommandStream::Pages(range) => assert!(range.bytes > 0, "a command run covers bytes"),
            CommandStream::Inline(bytes) => assert!(!bytes.is_empty(), "a stream has bytes"),
        }
        if self.refuse_before_post.load(Ordering::Relaxed) {
            return Err(Gpu3dError::Unspecified);
        }
        posted();
        let hold = self.submit_hold.lock().expect("no test panics here").take();
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        if self.transport_fault_submits.load(Ordering::Relaxed) {
            return Err(Gpu3dError::Transport(IoError::DeviceFault));
        }
        if *self.refuse_context.lock().expect("no test panics here") == Some(submission.context) {
            return Err(Gpu3dError::Unspecified);
        }
        Ok(())
    }

    async fn fences(
        &self,
        _context: ContextId,
        _ring: RingIndex,
        after: FenceId,
    ) -> Gpu3dResult<FenceId> {
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
    control: &ProviderReceiver<ControlMessage>,
    submit: &ProviderReceiver<SubmitMessage>,
    work: impl Future<Output = T>,
) -> T {
    let cpu = TestCpu::without_entropy();
    block_on(async {
        let work = pin!(work);
        let servers = pin!(async {
            futures::future::join(
                serve_control(device, shared, control),
                serve_submit(device, submit, &cpu),
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

/// An instance's open session, whose views go in a set of `views`.
fn opened_in(shared: &Arc<Gpu3dShared>, views: &mut ViewTable) -> Gpu3dOwnership {
    let service = Gpu3dService::from_shared(shared.clone());
    let mut ownership = Gpu3dOwnership::new();
    ownership
        .open(&service, || views.group(), test_caps::gpu())
        .expect("the engine renders");
    ownership
}

/// An open session for a test that shows nothing.
fn opened(shared: &Arc<Gpu3dShared>) -> Gpu3dOwnership {
    opened_in(shared, &mut ViewTable::new())
}

/// The queue into the owner tasks for `ownership`'s session.
fn sender(ownership: &Gpu3dOwnership) -> Gpu3dSender {
    ownership
        .session_ref()
        .expect("the session is open")
        .sender()
}

/// End `ownership`'s session, ending every view it showed in `views`.
fn close(ownership: &mut Gpu3dOwnership, views: &mut ViewTable) {
    let id = ownership.session_ref().expect("the session is open").id();
    ownership
        .close(id, views)
        .expect("the test pool refills anything");
}

/// Ask the control server to open a context of `capset` with `rings`
/// rings for `ownership`.
async fn request_context(
    ownership: &Gpu3dOwnership,
    capset: CapsetId,
    rings: u8,
) -> Result<ContextRecord, Gpu3dServiceError> {
    let (reply, answer) = oneshot::channel();
    sender(ownership)
        .control(
            Gpu3dRequest::CreateContext {
                capset,
                rings: RingCount::new(rings).expect("the test names a ring count in range"),
                name: ContextName::new(),
                reply,
            },
            answer,
        )
        .await
}

/// Open one single-ring context for `ownership`.
async fn create_context(ownership: &Gpu3dOwnership) -> ContextRecord {
    request_context(ownership, CAPSET, 1)
        .await
        .expect("the fake engine agrees")
}

/// Ask the control server for a blob of `memory` in `context`, with a
/// run of its own behind it when it is guest-backed.
async fn request_blob(
    ownership: &Gpu3dOwnership,
    context: ContextId,
    memory: BlobMemory,
    usage: BlobUsage,
    blob_id: u64,
) -> Result<BlobId, Gpu3dServiceError> {
    request_shared_blob(ownership, context, memory, usage, blob_id)
        .await
        .map(|blob| blob.id())
}

/// [`request_blob`], answered with the record a scanout holds the blob
/// through.
async fn request_shared_blob(
    ownership: &Gpu3dOwnership,
    context: ContextId,
    memory: BlobMemory,
    usage: BlobUsage,
    blob_id: u64,
) -> Result<Arc<BlobShared>, Gpu3dServiceError> {
    let backing = match memory {
        BlobMemory::Host3d => None,
        BlobMemory::Guest | BlobMemory::Host3dGuest => Some(pinned(4096)),
    };
    let (reply, answer) = oneshot::channel();
    sender(ownership)
        .control(
            Gpu3dRequest::CreateBlob {
                spec: BlobSpec {
                    context,
                    memory,
                    usage,
                    size: 4096,
                    blob_id,
                    backing,
                },
                reply,
            },
            answer,
        )
        .await
}

/// [`request_blob`], for a blob the fake engine makes.
async fn new_blob(
    ownership: &Gpu3dOwnership,
    context: ContextId,
    memory: BlobMemory,
    usage: BlobUsage,
) -> BlobId {
    request_blob(ownership, context, memory, usage, 0)
        .await
        .expect("the fake engine agrees")
}

/// Place `blob` in the aperture and report where, and how.
async fn place(ownership: &Gpu3dOwnership, blob: BlobId) -> DeviceRegion {
    let (reply, answer) = oneshot::channel();
    sender(ownership)
        .control(Gpu3dRequest::MapBlob { blob, reply }, answer)
        .await
        .expect("the fake engine agrees")
}

/// Queue the destroy of `blob` without waiting for it to be served.
async fn queue_destroy(ownership: &Gpu3dOwnership, blob: BlobId) {
    sender(ownership)
        .control_oneway(Gpu3dRequest::DestroyBlob { blob })
        .await
        .expect("the owner task is serving");
}

/// One page of command stream, pinned.
fn page_commands() -> SubmitCommands {
    let buffer = pinned(4096);
    let range = buffer.physical();
    SubmitCommands::Pages { buffer, range }
}

/// Admit and queue one submission the way the host call does: the fence
/// is checked against its ring and recorded on it in the step that
/// queues it.
fn queue_submit(
    sender: &Gpu3dSender,
    context: &Arc<ContextShared>,
    ring: u8,
    fence: Option<u64>,
    commands: SubmitCommands,
) -> Result<oneshot::Receiver<Result<(), Gpu3dServiceError>>, Gpu3dServiceError> {
    let (ring, fence, timeline) = context.admit(ring, fence)?;
    let (reply, answer) = oneshot::channel();
    let queued = sender.try_submit(SubmitRequest {
        context: context.clone(),
        commands,
        ring,
        fence,
        reply,
    })?;
    assert!(queued, "no test fills the submit queue");
    if let Some(fence) = fence {
        timeline.record_submitted(fence);
    }
    Ok(answer)
}

/// Submit one page of commands on `ring` of `context` and await the
/// answer: `Ok` once the command is on the device's ring.
async fn submit_on(
    ownership: &Gpu3dOwnership,
    context: &ContextRecord,
    ring: u8,
    fence: Option<u64>,
) -> Result<(), Gpu3dServiceError> {
    let answer = queue_submit(
        &sender(ownership),
        &context.shared,
        ring,
        fence,
        page_commands(),
    )?;
    answer.await.unwrap_or(Err(Gpu3dServiceError::SessionEnded))
}

/// The newest fence retired on `ring` of `context`.
fn retired(context: &ContextShared, ring: u8) -> u64 {
    context
        .timeline(RingIndex::new(ring).expect("the test names a ring in range"))
        .expect("the context has the ring")
        .signal()
        .sequence()
}

/// Let the owner tasks run until they have nothing left to do.
async fn settle() {
    for _ in 0..16 {
        crate::yield_now().await;
    }
}

/// A device that renders nothing still answers the open — with
/// `no-renderer` rather than `unavailable`, because the machine has a
/// display engine; it simply has no renderer on it.
#[test]
fn an_engine_that_renders_nothing_cannot_be_opened() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(false);
    let service = Gpu3dService::from_shared(shared);

    assert_eq!(
        service.open(test_caps::gpu()).err(),
        Some(Gpu3dServiceError::NoRenderer)
    );
}

/// Nothing about the engine is exclusive: two instances open sessions
/// at once and each holds a context of its own on the same renderer.
#[test]
fn two_instances_hold_contexts_at_once() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let first = opened(&shared);
    let second = opened(&shared);
    assert_ne!(
        sender(&first).session(),
        sender(&second).session(),
        "each instance has a session of its own"
    );

    with_servers(&device, &shared, &control, &submit, async {
        let (a, b) = futures::future::join(create_context(&first), create_context(&second)).await;
        assert_ne!(a.shared.id(), b.shared.id());

        assert_eq!(submit_on(&first, &a, 0, Some(1)).await, Ok(()));
        assert_eq!(submit_on(&second, &b, 0, Some(1)).await, Ok(()));
        settle().await;
        assert_eq!(retired(&a.shared, 0), 1);
        assert_eq!(retired(&b.shared, 0), 1);
    });
}

/// One instance opens one session however many `gpu` handles it holds:
/// a second open is another handle on the first, and the session ends
/// only with the last.
#[test]
fn a_second_open_in_one_instance_shares_its_session() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let service = Gpu3dService::from_shared(shared);
    let mut views = ViewTable::new();
    let mut ownership = Gpu3dOwnership::new();

    let first = ownership
        .open(&service, || views.group(), test_caps::gpu())
        .expect("the engine renders");
    let second = ownership
        .open(&service, || views.group(), test_caps::gpu())
        .expect("the engine renders");
    assert_eq!(first, second, "both handles name one session");

    ownership
        .close(first, &mut views)
        .expect("the session showed nothing");
    assert!(ownership.is_open(), "one handle still holds the session");
    ownership
        .close(second, &mut views)
        .expect("the session showed nothing");
    assert!(!ownership.is_open(), "the last handle ends it");
}

/// Ending one instance's session hands back what that session made and
/// nothing another session made: the other instance's context, blob and
/// buffer stay, and it keeps working.
#[test]
fn one_instance_s_end_releases_only_its_own_resources() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut views = ViewTable::new();
    let mut dying = opened_in(&shared, &mut views);
    let living = opened(&shared);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let dead = create_context(&dying).await;
        let kept = create_context(&living).await;
        let dead_blob = new_blob(
            &dying,
            dead.shared.id(),
            BlobMemory::Guest,
            BlobUsage::empty(),
        )
        .await;
        let kept_blob = new_blob(
            &living,
            kept.shared.id(),
            BlobMemory::Guest,
            BlobUsage::empty(),
        )
        .await;
        assert_eq!(test_hooks::live_runs() - runs, 2);

        // Dying is this with the store's views withdrawn first.
        close(&mut dying, &mut views);
        settle().await;

        assert!(device.saw(&Call::DestroyBlob(dead_blob)));
        assert!(device.saw(&Call::DestroyContext(dead.shared.id())));
        assert!(!device.saw(&Call::DestroyBlob(kept_blob)));
        assert!(!device.saw(&Call::DestroyContext(kept.shared.id())));
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "only the ended session's backing went back to the pool"
        );

        assert_eq!(submit_on(&living, &kept, 0, Some(1)).await, Ok(()));
        new_blob(
            &living,
            kept.shared.id(),
            BlobMemory::Host3d,
            BlobUsage::MAPPABLE,
        )
        .await;
    });
}

/// An instance holds at most its share of contexts and of blobs, each
/// refused with its own word once the share is used up.
#[test]
fn an_instance_holds_at_most_its_share_of_contexts_and_blobs() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let mut contexts = Vec::new();
        for _ in 0..MAX_GPU_CONTEXTS {
            contexts.push(create_context(&ownership).await);
        }
        assert_eq!(
            request_context(&ownership, CAPSET, 1).await.err(),
            Some(Gpu3dServiceError::TooManyContexts)
        );

        let context = contexts[0].shared.id();
        for _ in 0..MAX_GPU_BLOBS {
            new_blob(&ownership, context, BlobMemory::Host3d, BlobUsage::MAPPABLE).await;
        }
        assert_eq!(
            request_blob(
                &ownership,
                context,
                BlobMemory::Host3d,
                BlobUsage::MAPPABLE,
                0
            )
            .await
            .err(),
            Some(Gpu3dServiceError::TooManyBlobs)
        );
    });
}

/// The machine's tables are bounded too, and a session under its own
/// share is refused with the machine's word once every slot is taken.
#[test]
fn the_machine_holds_at_most_its_tables_of_contexts_and_blobs() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let full_sessions = MAX_MACHINE_GPU_CONTEXTS / MAX_GPU_CONTEXTS;
    let sessions: Vec<_> = (0..full_sessions).map(|_| opened(&shared)).collect();
    let late = opened(&shared);
    assert_eq!(
        MAX_MACHINE_GPU_BLOBS / MAX_GPU_BLOBS,
        full_sessions,
        "the blob table fills at the same session count the context table does"
    );

    with_servers(&device, &shared, &control, &submit, async {
        let mut contexts = Vec::new();
        for session in &sessions {
            let mut own = Vec::new();
            for _ in 0..MAX_GPU_CONTEXTS {
                own.push(create_context(session).await);
            }
            contexts.push(own);
        }
        assert_eq!(
            request_context(&late, CAPSET, 1).await.err(),
            Some(Gpu3dServiceError::ContextsExhausted),
            "a session holding nothing is refused once the machine's table is full"
        );

        // A context to put blobs in for the late session: one of the
        // full ones gives a context back.
        let given_back = contexts[0].pop().expect("the session holds contexts");
        sender(&sessions[0])
            .control_oneway(Gpu3dRequest::DestroyContext {
                context: given_back.shared.id(),
            })
            .await
            .expect("the owner task is serving");
        let late_context = create_context(&late).await;

        for (session, own) in sessions.iter().zip(&contexts) {
            let context = own.first().expect("the session holds contexts").shared.id();
            for _ in 0..MAX_GPU_BLOBS {
                new_blob(session, context, BlobMemory::Host3d, BlobUsage::MAPPABLE).await;
            }
        }
        assert_eq!(
            request_blob(
                &late,
                late_context.shared.id(),
                BlobMemory::Host3d,
                BlobUsage::MAPPABLE,
                0,
            )
            .await
            .err(),
            Some(Gpu3dServiceError::BlobsExhausted),
            "a session holding no blob is refused once the machine's table is full"
        );
    });
}

/// Each ring is its own timeline: a fence is stale only against the
/// fences already let onto its own ring, and an unfenced submission is
/// never stale.
#[test]
fn a_stale_fence_is_checked_per_ring() {
    let context = ContextShared::new(
        ContextId::new(1),
        RingCount::new(2).expect("two rings are in range"),
    );
    let (ring, fence, timeline) = context.admit(0, Some(5)).expect("the ring is fresh");
    assert_eq!((ring.raw(), fence), (0, Some(FenceId::new(5))));
    timeline.record_submitted(FenceId::new(5));

    assert_eq!(
        context.admit(0, Some(5)).err(),
        Some(Gpu3dServiceError::StaleFence)
    );
    assert_eq!(
        context.admit(0, Some(4)).err(),
        Some(Gpu3dServiceError::StaleFence)
    );
    assert!(
        context.admit(1, Some(1)).is_ok(),
        "ring 1 has admitted nothing yet"
    );
    assert!(
        context.admit(0, None).is_ok(),
        "no fence, nothing to be stale"
    );
    assert!(context.admit(0, Some(6)).is_ok());
}

/// A ring past the context's count is `bad-ring`, and so is one past the
/// most any context has.
#[test]
fn a_ring_the_context_was_not_created_with_is_a_bad_ring() {
    let context = ContextShared::new(
        ContextId::new(1),
        RingCount::new(2).expect("two rings are in range"),
    );
    for ring in [2, 63, 64, u8::MAX] {
        assert_eq!(
            context.admit(ring, None).err(),
            Some(Gpu3dServiceError::BadRing),
            "ring {ring}"
        );
    }
    assert!(
        RingCount::new(0).is_none(),
        "a context has at least one ring"
    );
    assert!(
        RingCount::new(65).is_none(),
        "and at most the protocol's 64"
    );
}

/// A context's rings reach the device at create, and every submission
/// names its ring; each ring's fences retire on that ring alone.
#[test]
fn fences_retire_on_the_ring_they_were_submitted_on() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let context = request_context(&ownership, CAPSET, 2)
            .await
            .expect("the fake engine agrees");
        assert!(device.saw(&Call::CreateContext(CAPSET, 2)));

        assert_eq!(submit_on(&ownership, &context, 0, Some(3)).await, Ok(()));
        assert_eq!(submit_on(&ownership, &context, 1, Some(1)).await, Ok(()));
        assert_eq!(submit_on(&ownership, &context, 1, None).await, Ok(()));
        settle().await;

        assert_eq!(retired(&context.shared, 0), 3);
        assert_eq!(retired(&context.shared, 1), 1);
        let id = context.shared.id();
        assert!(device.saw(&Call::Submit(id, 0, Some(FenceId::new(3)))));
        assert!(device.saw(&Call::Submit(id, 1, Some(FenceId::new(1)))));
        assert!(device.saw(&Call::Submit(id, 1, None)));
    });
}

/// A short stream travels in the request itself and reaches the device
/// as the bytes the guest gave.
#[test]
fn an_inline_stream_reaches_the_device() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let inline = InlineCommands::try_from([1_u8, 2, 3, 4].as_slice()).expect("four bytes fit");
        let answer = queue_submit(
            &sender(&ownership),
            &context.shared,
            0,
            Some(2),
            SubmitCommands::Inline(inline),
        )
        .expect("the ring admits the fence");
        assert_eq!(answer.await, Ok(Ok(())));
        settle().await;
        assert_eq!(retired(&context.shared, 0), 2);
    });
}

/// A refusal the device answers after the submission resolved faults
/// that context and no other: every ring of it retires up to its newest
/// fence — a chain still with the device included — later submissions
/// on it are refused `device-fault`, and another context on the same
/// engine carries on.
#[test]
fn a_refusal_faults_only_its_context_and_retires_every_ring() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);
    let other = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let faulted = request_context(&ownership, CAPSET, 2)
            .await
            .expect("the fake engine agrees");
        let healthy = create_context(&other).await;

        // Ring 1's chain is posted and stays with the device; then the
        // device refuses the next command on ring 0.
        let (gate, hold) = oneshot::channel();
        *device.submit_hold.lock().expect("no test panics here") = Some(hold);
        let parked = queue_submit(
            &sender(&ownership),
            &faulted.shared,
            1,
            Some(9),
            page_commands(),
        )
        .expect("the ring admits the fence");
        assert_eq!(parked.await, Ok(Ok(())), "resolved once it is on the ring");
        *device.refuse_context.lock().expect("no test panics here") = Some(faulted.shared.id());
        assert_eq!(
            submit_on(&ownership, &faulted, 0, Some(2)).await,
            Ok(()),
            "the refusal comes after the submission resolved"
        );
        settle().await;

        assert!(faulted.shared.is_faulted());
        assert_eq!(retired(&faulted.shared, 0), 2);
        assert_eq!(
            retired(&faulted.shared, 1),
            9,
            "a ring whose chain the device still holds retires too"
        );
        assert_eq!(
            submit_on(&ownership, &faulted, 0, Some(3)).await,
            Err(Gpu3dServiceError::DeviceFault)
        );

        assert!(!healthy.shared.is_faulted());
        assert_eq!(submit_on(&other, &healthy, 0, Some(1)).await, Ok(()));
        settle().await;
        assert_eq!(retired(&healthy.shared, 0), 1);

        gate.send(()).expect("the device is parked on it");
        settle().await;
    });
}

/// A submission the driver could not put on the ring is answered with
/// the refusal itself, and its fence still retires.
#[test]
fn a_submission_the_driver_could_not_post_is_answered_with_its_refusal() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    device.refuse_before_post.store(true, Ordering::Relaxed);
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        assert_eq!(
            submit_on(&ownership, &context, 0, Some(4)).await,
            Err(Gpu3dServiceError::DeviceFault)
        );
        assert_eq!(retired(&context.shared, 0), 4, "its fence retired anyway");
        assert!(context.shared.is_faulted());
    });
}

/// A pure guest blob has no host allocation for a blob id to name: one
/// that names one is refused before the device is asked. A host blob
/// carries the id through.
#[test]
fn a_guest_blob_naming_a_blob_id_is_refused() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let id = context.shared.id();
        assert_eq!(
            request_blob(&ownership, id, BlobMemory::Guest, BlobUsage::empty(), 5)
                .await
                .err(),
            Some(Gpu3dServiceError::InvalidBlob)
        );
        request_blob(&ownership, id, BlobMemory::Host3d, BlobUsage::MAPPABLE, 5)
            .await
            .expect("a host blob names its allocation");
    });

    let created: Vec<_> = device
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::CreateBlob(..)))
        .collect();
    assert_eq!(
        created,
        alloc::vec![Call::CreateBlob(
            ContextId::new(1),
            BlobMemory::Host3d,
            4096,
            5
        )],
        "only the host blob reached the device"
    );
}

/// A placement the device reports write-combined is shown in the
/// instance's memory under that memory type.
#[test]
fn a_write_combining_blob_is_shown_write_combined() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let mut device = FakeGpu3d::new();
    device.map_attributes = DeviceRegionAttributes::WRITE_COMBINING_MEMORY;
    let mut views = ViewTable::new();
    let ownership = opened_in(&shared, &mut views);

    let region = with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let blob = new_blob(
            &ownership,
            context.shared.id(),
            BlobMemory::Host3d,
            BlobUsage::MAPPABLE,
        )
        .await;
        place(&ownership, blob).await
    });
    assert_eq!(
        region.attributes,
        DeviceRegionAttributes::WRITE_COMBINING_MEMORY
    );

    let before = test_hooks::changes().len();
    views
        .show_device(
            memory(),
            pages(2, 1),
            region,
            Some(ownership.views().expect("an open session has a set")),
        )
        .expect("the span is free");
    assert_eq!(
        &test_hooks::changes()[before..],
        &[test_hooks::MappingChange::SubstituteDevice(
            helios_hal::vmm::VirtRange::new(
                VirtAddr::new(MEMORY_BASE + 2 * WASM_PAGE_BYTES as usize),
                0x1000
            ),
            DeviceRegionAttributes::WRITE_COMBINING_MEMORY,
        )],
        "the aperture is shown under the memory type the device reported"
    );
}

/// Ending the session ends every view it showed — command buffers,
/// guest blobs and aperture placements alike — and a run shown nowhere
/// and held by nothing else goes back to the pool with it.
#[test]
fn ending_the_session_ends_every_view_in_its_set() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let mut views = ViewTable::new();
    let mut ownership = opened_in(&shared, &mut views);
    let group = ownership.views().expect("an open session has a set");
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
                PhysicalRange::new(APERTURE, 0x1000),
                DeviceRegionAttributes::PREFETCHABLE_MEMORY,
            ),
            Some(group),
        )
        .expect("the span is free");
    let before = test_hooks::changes().len();

    close(&mut ownership, &mut views);

    assert!(views.is_empty(), "nothing of the session is shown");
    assert!(!ownership.is_open());
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

/// A session shows at most its share of the store's views: the next one
/// is refused before anything is substituted, and ending one makes room.
#[test]
fn a_session_shows_at_most_its_share_of_views() {
    test_hooks::install();
    let (shared, _control, _submit) = gpu3d_channels(true);
    let mut views = ViewTable::new();
    let ownership = opened_in(&shared, &mut views);
    let region = DeviceRegion::new(
        PhysicalRange::new(APERTURE, 0x1000),
        DeviceRegionAttributes::PREFETCHABLE_MEMORY,
    );
    let mut shown = Vec::new();
    for page in 0..MAX_GPU_VIEWS as u64 {
        let group = ownership
            .room_for_view(&views)
            .expect("the session is under its share");
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
        .end(shown.pop().expect("the session shows something"))
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
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        assert_eq!(
            request_context(&ownership, CapsetId::GFXSTREAM_VULKAN, 1)
                .await
                .err(),
            Some(Gpu3dServiceError::UnsupportedContext)
        );
    });

    assert_eq!(
        device.calls(),
        alloc::vec![Call::Capsets],
        "the context never reached the device"
    );
}

/// A capability set is the renderer's bytes, fetched whole at the
/// version asked for and never interpreted; a set the device does not
/// carry, or a version past the newest it publishes, is no such set.
#[test]
fn a_capset_s_bytes_reach_their_reader_undecoded() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let read = |id, version| {
            let (reply, answer) = oneshot::channel();
            let sender = sender(&ownership);
            async move {
                sender
                    .control(Gpu3dRequest::Capset { id, version, reply }, answer)
                    .await
            }
        };
        assert_eq!(
            read(CAPSET, CAPSET_MAX_VERSION).await,
            Ok(alloc::vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(
            read(CAPSET, CAPSET_MAX_VERSION + 1).await.err(),
            Some(Gpu3dServiceError::NoSuchCapset)
        );
        assert_eq!(
            read(CapsetId::VIRGL, 0).await.err(),
            Some(Gpu3dServiceError::NoSuchCapset)
        );
    });

    assert_eq!(
        device
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Capset(..)))
            .collect::<Vec<_>>(),
        alloc::vec![Call::Capset(CAPSET, CAPSET_MAX_VERSION)],
        "only the set the device carries, at a version it publishes, was read"
    );
}

/// A blob names the context whose renderer owns it; a context the
/// session never opened is not a place a resource can live.
#[test]
fn a_blob_for_a_context_the_session_never_opened_is_refused() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);
    let other = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let foreign = create_context(&other).await;
        for context in [ContextId::new(77), foreign.shared.id()] {
            assert_eq!(
                request_blob(
                    &ownership,
                    context,
                    BlobMemory::Guest,
                    BlobUsage::empty(),
                    0
                )
                .await
                .err(),
                Some(Gpu3dServiceError::InvalidBlob)
            );
        }
    });

    assert!(
        !device
            .calls()
            .iter()
            .any(|call| matches!(call, Call::CreateBlob(..))),
        "the blob never reached the device"
    );
}

/// Ending the session gives the aperture placements back before the
/// blobs, and the blobs before the contexts: a renderer still holding a
/// context that names dead resources would be reading guest pages on
/// their way back to a pool. The blob's buffer goes back only after the
/// device has destroyed the blob, because the owner task's record holds
/// it.
#[test]
fn an_ended_session_unmaps_then_destroys_then_frees() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut views = ViewTable::new();
    let mut ownership = opened_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let blob = new_blob(
            &ownership,
            context.shared.id(),
            BlobMemory::Host3dGuest,
            BlobUsage::MAPPABLE,
        )
        .await;
        place(&ownership, blob).await;
        assert_eq!(test_hooks::live_runs() - runs, 1);

        close(&mut ownership, &mut views);
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the blob's buffer is not freed by the end itself"
        );
        settle().await;
    });

    let calls = device.calls();
    let position = |wanted: fn(&Call) -> bool| {
        calls
            .iter()
            .position(wanted)
            .expect("the release made the call")
    };
    let unmap = position(|call| matches!(call, Call::UnmapBlob(_)));
    let destroy_blob = position(|call| matches!(call, Call::DestroyBlob(_)));
    let destroy_context = position(|call| matches!(call, Call::DestroyContext(_)));
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
}

/// A request made under a session that has ended is dropped rather than
/// served: a dead plugin must not drive the renderer.
#[test]
fn a_request_that_outlived_its_session_is_not_served() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut ownership = opened(&shared);
    let stale = sender(&ownership);
    let context = Arc::new(ContextShared::new(
        ContextId::new(1),
        RingCount::new(1).expect("one ring is in range"),
    ));

    with_servers(&device, &shared, &control, &submit, async {
        close(&mut ownership, &mut ViewTable::new());
        settle().await;
        let _next = opened(&shared);

        let (reply, answer) = oneshot::channel();
        let refused = stale.control(Gpu3dRequest::Capsets { reply }, answer).await;
        assert_eq!(refused.err(), Some(Gpu3dServiceError::Closed));

        let answer =
            queue_submit(&stale, &context, 0, Some(1), page_commands()).expect("it is admitted");
        assert!(
            answer.await.is_err(),
            "the submission is dropped unanswered"
        );
    });

    assert!(
        device.calls().is_empty(),
        "nothing reached the device under a session that had ended"
    );
}

/// A request sitting in the inbox when its session ends is dropped
/// rather than served.
#[test]
fn a_request_queued_before_the_end_is_dropped() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut ownership = opened(&shared);
    let sender = sender(&ownership);

    // The request lands in the inbox before the servers run and before
    // the session ends: it is the one thing the owner task finds
    // waiting when it wakes for the end.
    let (reply, answer) = oneshot::channel();
    let mut pending = pin!(sender.control(
        Gpu3dRequest::CreateContext {
            capset: CAPSET,
            rings: RingCount::new(1).expect("one ring is in range"),
            name: ContextName::new(),
            reply,
        },
        answer,
    ));
    assert!(
        block_on(poll_once(pending.as_mut())).is_none(),
        "the request is queued and unanswered"
    );
    close(&mut ownership, &mut ViewTable::new());

    with_servers(&device, &shared, &control, &submit, async {
        assert_eq!(
            pending.await.err(),
            Some(Gpu3dServiceError::Closed),
            "the ended session's request is dropped, not served"
        );
        settle().await;
    });

    assert!(
        device.calls().is_empty(),
        "no device-side context was created for a session that had ended"
    );
}

/// A submission the device is still working on is not torn down under
/// it: the release waits the in-flight tally out, so the destroy lands
/// on the control ring only after the device has finished the chain,
/// and the command buffer is not freed while the engine may still be
/// reading it — even once the instance's own view of it has ended.
#[test]
fn a_submission_in_flight_is_finished_before_the_session_is_released() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let (gate, hold) = oneshot::channel();
    *device.submit_hold.lock().expect("no test panics here") = Some(hold);
    let mut views = ViewTable::new();
    let mut ownership = opened_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let buffer = pinned(4096);
        let range = buffer.physical();
        views
            .show_buffer(
                memory(),
                pages(0, 1),
                &buffer,
                Some(ownership.views().expect("an open session has a set")),
            )
            .expect("the span is free");

        // The device takes the submission and keeps working on it, as a
        // chain parked for ring room is. The request's reference is the
        // only one left once the view ends.
        let answer = queue_submit(
            &sender(&ownership),
            &context.shared,
            0,
            Some(1),
            SubmitCommands::Pages { buffer, range },
        )
        .expect("the ring admits the fence");
        assert_eq!(answer.await, Ok(Ok(())), "resolved once it is on the ring");
        let submitted = Call::Submit(context.shared.id(), 0, Some(FenceId::new(1)));
        assert!(device.saw(&submitted), "the device is holding the chain");

        // The session ends while the device holds it, and with it the
        // instance's view of the command buffer. The teardown is given
        // every chance to run ahead of the device.
        close(&mut ownership, &mut views);
        assert!(views.is_empty(), "the session's views end with it");
        settle().await;
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

        // The device finishes; only then may the session's teardown run.
        gate.send(()).expect("the device is parked on it");
        settle().await;
        assert!(device.saw(&Call::DestroyContext(context.shared.id())));
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

/// An ended session whose submission the device still holds does not
/// hold up the control server: another session's creates are served
/// while the release waits, and the ended session's context goes only
/// once the device has answered.
#[test]
fn an_ended_session_s_drain_does_not_stall_other_sessions() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let (gate, hold) = oneshot::channel();
    *device.submit_hold.lock().expect("no test panics here") = Some(hold);
    let mut views = ViewTable::new();
    let mut ending = opened_in(&shared, &mut views);
    let other = opened(&shared);

    with_servers(&device, &shared, &control, &submit, async {
        let held_back = create_context(&ending).await;
        assert_eq!(
            submit_on(&ending, &held_back, 0, Some(1)).await,
            Ok(()),
            "posted, and still with the device"
        );
        close(&mut ending, &mut views);
        settle().await;

        let context = create_context(&other).await;
        new_blob(
            &other,
            context.shared.id(),
            BlobMemory::Host3d,
            BlobUsage::MAPPABLE,
        )
        .await;
        assert!(
            !device.saw(&Call::DestroyContext(held_back.shared.id())),
            "the ended session is still draining"
        );

        gate.send(()).expect("the device is parked on it");
        settle().await;
        assert!(
            device.saw(&Call::DestroyContext(held_back.shared.id())),
            "the drain resumed the release"
        );
        assert!(!device.saw(&Call::DestroyContext(context.shared.id())));
    });
}

/// A blob a scanout shows outlives its handle: the destroy its handle
/// asked for, and its session's end, wait for the scanout to let go,
/// and only then is the blob destroyed — and its context after it.
#[test]
fn a_scanned_out_blob_is_destroyed_only_once_the_scanout_lets_go() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let mut views = ViewTable::new();
    let mut ownership = opened_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    let blob_id = with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let blob = request_shared_blob(
            &ownership,
            context.shared.id(),
            BlobMemory::Guest,
            BlobUsage::empty(),
            0,
        )
        .await
        .expect("the fake engine agrees");
        // What a blob surface carries while it is presented.
        let scanout = ScanoutHold::new(&blob);

        // The handle goes first.
        queue_destroy(&ownership, blob.id()).await;
        settle().await;
        assert!(
            !device.saw(&Call::DestroyBlob(blob.id())),
            "no destroy reaches the device while the scanout shows the blob"
        );
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the blob's pages stay pinned"
        );

        // The surface goes: the display path drops its hold once the
        // scanout is blanked.
        drop(scanout);
        settle().await;
        assert!(device.saw(&Call::DestroyBlob(blob.id())));
        assert_eq!(test_hooks::live_runs(), runs, "and its pages go back");

        // The same through the session's end: a second blob, shown, and
        // then the whole session goes.
        let second = request_shared_blob(
            &ownership,
            context.shared.id(),
            BlobMemory::Guest,
            BlobUsage::empty(),
            0,
        )
        .await
        .expect("the fake engine agrees");
        let scanout = ScanoutHold::new(&second);
        close(&mut ownership, &mut views);
        settle().await;
        assert!(!device.saw(&Call::DestroyBlob(second.id())));
        assert!(
            !device.saw(&Call::DestroyContext(context.shared.id())),
            "nor does its context's, which goes after it"
        );
        drop(scanout);
        settle().await;
        second.id()
    });

    let calls = device.calls();
    let destroyed = calls
        .iter()
        .position(|call| *call == Call::DestroyBlob(blob_id))
        .expect("the blob is destroyed once the scanout let go");
    let context_gone = calls
        .iter()
        .position(|call| matches!(call, Call::DestroyContext(_)))
        .expect("and then its context");
    assert!(destroyed < context_gone);
    assert_eq!(test_hooks::live_runs(), runs);
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

/// A blob's run stays live for as long as the device has not answered
/// the destroy, and goes back once it has.
#[test]
fn a_blob_is_freed_only_once_the_device_has_destroyed_it() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let (gate, hold) = oneshot::channel();
    *device.destroy_hold.lock().expect("no test panics here") = Some(hold);
    let ownership = opened(&shared);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let blob = new_blob(
            &ownership,
            context.shared.id(),
            BlobMemory::Guest,
            BlobUsage::empty(),
        )
        .await;
        assert_eq!(test_hooks::live_runs() - runs, 1);

        queue_destroy(&ownership, blob).await;
        settle().await;
        assert!(
            device.saw(&Call::DestroyBlob(blob)),
            "the device is holding the destroy"
        );
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the run stays live while the device has not answered"
        );

        gate.send(()).expect("the device is parked on it");
        settle().await;
        assert_eq!(
            test_hooks::live_runs(),
            runs,
            "the run goes back once the device has destroyed the blob"
        );
    });
}

/// A blob the device would not destroy may still be read and written by
/// the renderer, so its run never goes back to the pool, whether the
/// destroy was asked for on its own or came with the session's end.
#[test]
fn a_blob_the_device_would_not_destroy_stays_pinned() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    device.refuse_destroy.store(true, Ordering::Relaxed);
    let mut views = ViewTable::new();
    let mut ownership = opened_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let id = context.shared.id();
        let first = new_blob(&ownership, id, BlobMemory::Guest, BlobUsage::empty()).await;
        new_blob(&ownership, id, BlobMemory::Guest, BlobUsage::empty()).await;
        assert_eq!(test_hooks::live_runs() - runs, 2);

        queue_destroy(&ownership, first).await;
        settle().await;
        assert!(device.saw(&Call::DestroyBlob(first)));
        assert_eq!(
            test_hooks::live_runs() - runs,
            2,
            "a refused destroy keeps the run"
        );

        close(&mut ownership, &mut views);
        settle().await;
    });

    let destroyed = device
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::DestroyBlob(_)))
        .count();
    assert_eq!(destroyed, 2, "both blobs were asked to go");
    assert_eq!(
        test_hooks::live_runs() - runs,
        2,
        "neither run goes back to a pool the renderer may still write"
    );
}

/// A blob the device would not take out of the aperture is not destroyed
/// at all, and its run never goes back to the pool.
#[test]
fn a_blob_the_device_would_not_unmap_stays_pinned() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    device.refuse_unmap.store(true, Ordering::Relaxed);
    let mut views = ViewTable::new();
    let mut ownership = opened_in(&shared, &mut views);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let id = context.shared.id();
        let first = new_blob(&ownership, id, BlobMemory::Host3dGuest, BlobUsage::MAPPABLE).await;
        let second = new_blob(&ownership, id, BlobMemory::Host3dGuest, BlobUsage::MAPPABLE).await;
        place(&ownership, first).await;
        place(&ownership, second).await;
        assert_eq!(test_hooks::live_runs() - runs, 2);

        queue_destroy(&ownership, first).await;
        settle().await;
        assert!(device.saw(&Call::UnmapBlob(first)));
        assert_eq!(
            test_hooks::live_runs() - runs,
            2,
            "a refused unmap keeps the run"
        );

        close(&mut ownership, &mut views);
        settle().await;
    });

    assert!(
        !device
            .calls()
            .iter()
            .any(|call| matches!(call, Call::DestroyBlob(_))),
        "a placed blob is never offered to the device's destroy"
    );
    assert_eq!(
        test_hooks::live_runs() - runs,
        2,
        "neither run goes back to a pool the renderer may still write"
    );
}

/// A submission the device never answered may still be read by it, so
/// its command buffer stays out of the pool; one the device answered
/// with a refusal is finished with, and the buffer goes back.
#[test]
fn a_command_buffer_is_kept_only_when_the_device_never_answered() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let refused = create_context(&ownership).await;
        let unanswered = create_context(&ownership).await;

        *device.refuse_context.lock().expect("no test panics here") = Some(refused.shared.id());
        assert_eq!(submit_on(&ownership, &refused, 0, Some(1)).await, Ok(()));
        settle().await;
        assert_eq!(
            test_hooks::live_runs(),
            runs,
            "an answered refusal frees the command buffer"
        );

        device
            .transport_fault_submits
            .store(true, Ordering::Relaxed);
        assert_eq!(submit_on(&ownership, &unanswered, 0, Some(2)).await, Ok(()));
        settle().await;
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "a submission the device never answered keeps its command buffer"
        );
        assert_eq!(
            retired(&unanswered.shared, 0),
            2,
            "its fence retired anyway"
        );
    });
}

/// A blob create the device never answered may have attached the pages
/// to a blob nobody has a name for, so the run stays out of the pool;
/// one the device refused attached nothing, and the run goes back.
#[test]
fn a_blobs_backing_is_kept_only_when_the_create_was_never_answered() {
    test_hooks::install();
    let (shared, control, submit) = gpu3d_channels(true);
    let device = FakeGpu3d::new();
    let ownership = opened(&shared);
    let runs = test_hooks::live_runs();

    with_servers(&device, &shared, &control, &submit, async {
        let context = create_context(&ownership).await;
        let id = context.shared.id();

        *device.fail_creates.lock().expect("no test panics here") = Some(false);
        let refused = request_blob(&ownership, id, BlobMemory::Guest, BlobUsage::empty(), 0).await;
        assert!(refused.is_err());
        settle().await;
        assert_eq!(
            test_hooks::live_runs(),
            runs,
            "an answered refusal frees the backing"
        );

        *device.fail_creates.lock().expect("no test panics here") = Some(true);
        let unanswered =
            request_blob(&ownership, id, BlobMemory::Guest, BlobUsage::empty(), 0).await;
        assert!(unanswered.is_err());
        settle().await;
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "a create the device never answered keeps its backing"
        );
    });
}

/// The guest-facing handles a component holds are the store's resource
/// table entries, so the stale-handle tests build a store for real: a
/// handle minted under one session names that session's resources, and
/// answered under the next it would be a run somebody else owns. The
/// fixture itself lives with the adapter, because the store type names
/// pieces that are adapter-only.
#[cfg(feature = "wasmtime-runtime")]
mod component_host {
    use wasmtime::component::Resource;

    use super::*;
    use crate::wasmtime_adapter::bindings::gpu::bindings::helios::system::gpu as contract;
    use crate::wasmtime_adapter::component_host::service::test_store;
    use crate::wasmtime_adapter::component_host::service::{BlobHandle, CommandBufferHandle};

    /// Show a fresh 4 KiB buffer at wasm page `page` of the store's
    /// memory, in the open session's set.
    fn shown(data: &mut test_store::TestStoreData, page: u64) -> (Buffer, crate::ViewId) {
        let group = data.device.gpu().views().expect("the session is open");
        let buffer = pinned(4096);
        let view = data
            .views
            .show_buffer(memory(), pages(page, 1), &buffer, Some(group))
            .expect("the span is free");
        (buffer, view)
    }

    /// End the store's session and open a new one, the way a plugin that
    /// let its last `gpu` handle go and opened the engine again does.
    fn reopen(
        data: &mut test_store::TestStoreData,
        service: &Gpu3dService,
        first: super::super::SessionId,
    ) -> super::super::SessionId {
        data.device
            .gpu_mut()
            .close(first, &mut data.views)
            .expect("the test pool refills anything");
        data.device
            .open_gpu(service, &mut data.views, test_caps::gpu())
            .expect("the engine renders")
    }

    /// A buffer a component kept across its session's end was shown in
    /// a set the end already ended, so `submit` refuses it with the
    /// contract's word for an ended session, and dropping it — or a blob
    /// of the same session — ends nothing twice. A live session's handle
    /// ends its own view when it is dropped.
    #[test]
    fn a_command_buffer_that_outlived_its_session_is_refused() {
        test_hooks::install();
        let (shared, _control, _submit) = gpu3d_channels(true);
        let service = Gpu3dService::from_shared(shared);
        let mut data = test_store::store_data(&service);
        let first = data
            .device
            .open_gpu(&service, &mut data.views, test_caps::gpu())
            .expect("the engine renders");
        let (buffer, view) = shown(&mut data, 0);
        let commands = data
            .table
            .push(CommandBufferHandle {
                session: first,
                buffer,
                view,
            })
            .expect("the table has room");
        // The run is shown; the owner task's record, which this test has
        // none of, is what would hold it.
        let (_storage, backing) = shown(&mut data, 1);
        let blob = data
            .table
            .push(BlobHandle {
                session: first,
                id: BlobId::new(9),
                shared: Arc::new(BlobShared::new(
                    BlobId::new(9),
                    Arc::new(crate::exec::Notify::new()),
                )),
                backing: Some(backing),
                mapped: None,
            })
            .expect("the table has room");

        let live = reopen(&mut data, &service, first);
        assert!(data.views.is_empty(), "the end ended the session's views");
        assert_ne!(live, first, "a new session is a new identity");

        let refused = data
            .command_range(&commands, 0, 16, live)
            .expect("the handle is still in the table");
        assert!(
            matches!(refused, Err(contract::Error::SessionEnded)),
            "submit's run check answers session-ended for an ended session's buffer"
        );
        assert!(
            contract::HostCommandBuffer::drop(&mut data, commands).is_ok(),
            "an ended session's buffer ends no view a second time"
        );
        assert!(
            contract::HostBlob::drop(&mut data, blob).is_ok(),
            "an ended session's blob ends no view a second time"
        );

        let (buffer, view) = shown(&mut data, 2);
        let commands = data
            .table
            .push(CommandBufferHandle {
                session: live,
                buffer,
                view,
            })
            .expect("the table has room");
        contract::HostCommandBuffer::drop(&mut data, commands)
            .expect("a live session's buffer ends its view");
        assert!(
            !data.views.is_shown(view),
            "the span is ordinary memory again"
        );
    }

    /// A context that outlived its session hands out no fence stream —
    /// nothing publishes to it again, so the guest's read would hang —
    /// and a live context refuses a ring it was not created with.
    #[test]
    fn a_context_opens_fence_streams_only_for_a_live_session_and_its_own_rings() {
        test_hooks::install();
        let (shared, _control, _submit) = gpu3d_channels(true);
        let service = Gpu3dService::from_shared(shared);
        let mut data = test_store::store_data(&service);
        let first = data
            .device
            .open_gpu(&service, &mut data.views, test_caps::gpu())
            .expect("the engine renders");
        let rings = RingCount::new(2).expect("two rings are in range");
        let stale = data
            .table
            .push(test_store::context_handle(
                first,
                Arc::new(ContextShared::new(ContextId::new(4), rings)),
            ))
            .expect("the table has room");

        let live = reopen(&mut data, &service, first);
        let live_context = data
            .table
            .push(test_store::context_handle(
                live,
                Arc::new(ContextShared::new(ContextId::new(5), rings)),
            ))
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
        let mut fences = |context: &Resource<_>, ring: u8| {
            contract::HostContextWithStore::fences(
                GpuAccess::new(store.as_context_mut(), |data| data),
                Resource::new_borrow(context.rep()),
                ring,
            )
            .expect("an answer, not a trap")
            .map(|_| ())
        };
        assert_eq!(fences(&stale, 0), Err(contract::Error::SessionEnded));
        assert_eq!(fences(&live_context, 2), Err(contract::Error::BadRing));
        assert_eq!(fences(&live_context, 1), Ok(()));
        assert_eq!(fences(&live_context, 0), Ok(()));
    }

    fn authority_with(classes: crate::DeviceClassRights) -> crate::ProcessAuthority {
        let mut authority = crate::ProcessAuthority::empty();
        authority.grant_device_classes(classes);
        authority
    }

    /// An ordinary program is told what a machine with no 3D service
    /// tells everyone, not `no-renderer`.
    #[test]
    fn ordinary_program_is_refused_the_gpu_as_if_there_were_none() {
        test_hooks::install();
        let (shared, _control, _submit) = gpu3d_channels(true);
        let service = Gpu3dService::from_shared(shared);

        for authority in [
            crate::ProcessAuthority::empty(),
            crate::ProcessAuthority::root(),
            authority_with(crate::DeviceClassRights::all() - crate::DeviceClassRights::GPU),
        ] {
            let mut data = test_store::store_data_authorized(authority, |runtime_state| {
                runtime_state.install_gpu3d_service(service.clone());
            });
            let answer = <test_store::TestStoreData as contract::Host>::open(&mut data)
                .expect("a refusal is an answer, not a trap");
            assert!(matches!(answer, Err(contract::Error::Unavailable)));
            assert!(!data.device.gpu().is_open());
        }

        let mut absent =
            test_store::store_data_authorized(crate::ProcessAuthority::empty(), |_| {});
        let none = <test_store::TestStoreData as contract::Host>::open(&mut absent)
            .expect("a machine with no 3D service answers");
        assert!(
            matches!(none, Err(contract::Error::Unavailable)),
            "the answer is the one a machine with no 3D service gives"
        );
    }

    /// A program that holds the GPU class opens the engine, and opening
    /// it twice holds one session through two handles.
    #[test]
    fn a_program_holding_the_gpu_class_opens_the_engine() {
        test_hooks::install();
        let (shared, _control, _submit) = gpu3d_channels(true);
        let service = Gpu3dService::from_shared(shared);
        let mut data = test_store::store_data_authorized(
            authority_with(crate::DeviceClassRights::GPU),
            |runtime_state| {
                runtime_state.install_gpu3d_service(service.clone());
            },
        );

        let first = <test_store::TestStoreData as contract::Host>::open(&mut data)
            .expect("the open is answered")
            .expect("the GPU class opens the engine");
        let second = <test_store::TestStoreData as contract::Host>::open(&mut data)
            .expect("the open is answered")
            .expect("a second handle is another hold on the session");
        assert!(data.device.gpu().is_open());

        contract::HostGpu::drop(&mut data, first).expect("the drop is answered");
        assert!(
            data.device.gpu().is_open(),
            "one handle still holds the session"
        );
        contract::HostGpu::drop(&mut data, second).expect("the drop is answered");
        assert!(!data.device.gpu().is_open(), "the last handle ends it");
    }
}
