extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use bytes::BytesMut;
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use fdt::Fdt;
use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{ExternalInterruptHandler, HostFsTransport, Kernel, Notify};
use plic::Plic;

use crate::RiscvCpu;
use crate::net::{InterruptSourceId, PlicContext};

pub(crate) const HOST_MOUNT_TAG: &str = helios_kernel::HOST_SHARE_MOUNT_TAG;

pub(crate) type HostFileSystemService = helios_kernel::HostFsClient<HostFsTransportService>;

#[derive(Clone)]
pub(crate) struct HostFsTransportService {
    inner: Arc<HostFsTransportServiceInner>,
}

struct HostFsTransportServiceInner {
    device: Arc<helios_virtio::VirtioMmio9pDevice>,
    requests: ConcurrentQueue<Request>,
    ready: Notify,
}

pub(crate) struct HostFsInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) transport: HostFsTransportService,
}

pub(crate) struct HostFsProbe {
    pub(crate) plic: &'static Plic,
    pub(crate) context: PlicContext,
    pub(crate) interrupt: HostFsInterrupt,
}

enum Request {
    Raw {
        bytes: Vec<u8>,
        response: BytesMut,
        response_len: usize,
        completion: oneshot::Sender<Result<BytesMut, IoError>>,
    },
}

pub(crate) fn install<WatchdogImpl>(
    cpu: &RiscvCpu,
    kernel: &Kernel<RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<HostFsProbe>
where
    WatchdogImpl: Watchdog + Clone,
{
    let Some((device, source)) = discover_9p_device(fdt) else {
        tracing::warn!("virtio 9p device was not discovered on the platform bus");
        return None;
    };
    let Some((plic, context)) =
        crate::net::discover_plic_context(fdt, cpu.bootstrap_processor().id())
    else {
        tracing::warn!("virtio 9p device was discovered but no PLIC context was available");
        return None;
    };

    let transport = HostFsTransportService {
        inner: Arc::new(HostFsTransportServiceInner {
            device,
            requests: ConcurrentQueue::unbounded(),
            ready: Notify::new(),
        }),
    };
    let runner = transport.clone();
    kernel.spawn_local_detached(async move {
        runner.run().await;
    });

    let service = HostFileSystemService::new(transport.clone());
    debug_state.install_host_fs_service(service);

    Some(HostFsProbe {
        plic,
        context,
        interrupt: HostFsInterrupt { source, transport },
    })
}

impl ExternalInterruptHandler for HostFsTransportService {
    fn handle_interrupt(&self) {
        self.inner.device.handle_interrupt();
    }
}

impl HostFsTransportService {
    async fn raw_request(
        &self,
        bytes: Vec<u8>,
        response: BytesMut,
        response_len: usize,
    ) -> Result<BytesMut, IoError> {
        tracing::info!("9p: enqueuing raw request ({} bytes)", bytes.len());
        let (completion, rx) = oneshot::channel();
        self.inner
            .requests
            .push(Request::Raw {
                bytes,
                response,
                response_len,
                completion,
            })
            .unwrap_or_else(|error| match error {
                PushError::Full(_) => unreachable!("host-fs request queue reported full"),
                PushError::Closed(_) => panic!("host-fs request queue was closed unexpectedly"),
            });
        self.inner.ready.notify_one();
        rx.await.unwrap_or_else(|_| {
            panic!("host-fs transport worker dropped completion channel unexpectedly")
        })
    }

    async fn run(&self) {
        tracing::info!("9p transport runner started");
        loop {
            let request = self.next_request().await;
            tracing::info!("9p transport: processing request");
            match request {
                Request::Raw {
                    bytes,
                    mut response,
                    response_len,
                    completion,
                } => {
                    response.clear();
                    response.resize(response_len, 0);
                    tracing::info!(
                        "9p transport: issuing device request ({} bytes)",
                        bytes.len()
                    );
                    let result = self
                        .inner
                        .device
                        .request(&bytes, &mut response)
                        .await
                        .and_then(|used| {
                            let used = usize::try_from(used).map_err(|_| IoError::DeviceFault)?;
                            response.truncate(used);
                            Ok(response)
                        });
                    tracing::info!(
                        "9p transport: device request completed, result={}",
                        result.is_ok()
                    );
                    let _ = completion.send(result);
                }
            }
        }
    }

    async fn next_request(&self) -> Request {
        loop {
            match self.inner.requests.pop() {
                Ok(request) => return request,
                Err(PopError::Empty) => self.inner.ready.notified().await,
                Err(PopError::Closed) => panic!("host-fs request queue was closed unexpectedly"),
            }
        }
    }
}

impl HostFsTransport for HostFsTransportService {
    fn mount_tag(&self) -> &str {
        self.inner.device.mount_tag()
    }

    fn request<'a>(
        &'a self,
        bytes: &'a [u8],
        response: &'a mut BytesMut,
        response_len: usize,
    ) -> impl core::future::Future<Output = Result<(), IoError>> + Send + 'a {
        // The riscv transport hands the request to a worker task
        // through `inner.requests`, so the bytes have to outlive the
        // current await. The kernel-side request buffer is owned by
        // the caller's pool, so the request bytes are copied into an
        // owned Vec while the caller-owned response buffer moves
        // through the worker and returns through the completion.
        let owned = bytes.to_vec();
        let mut worker_response = BytesMut::new();
        core::mem::swap(response, &mut worker_response);
        async move {
            let mut filled = self
                .raw_request(owned, worker_response, response_len)
                .await?;
            core::mem::swap(response, &mut filled);
            Ok(())
        }
    }
}

fn discover_9p_device(
    fdt: &Fdt<'_>,
) -> Option<(Arc<helios_virtio::VirtioMmio9pDevice>, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::_9P)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let irq_source = candidate
        .interrupt
        .and_then(|interrupt| core::num::NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-9p node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::p9_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-9p device at {base:#x}: {error}")
        });
    Some((Arc::new(device), irq_source))
}

pub(crate) fn has_9p_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::_9P) != 0
}
