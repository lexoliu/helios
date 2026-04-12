extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use core::num::NonZeroU32;
use fdt::Fdt;
use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use helios_kernel::{HostFsTransport, Kernel, Notify};
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
        response_len: usize,
        completion: oneshot::Sender<Result<Vec<u8>, IoError>>,
    },
}

pub(crate) fn install(
    cpu: &RiscvCpu,
    kernel: &Kernel<RiscvCpu>,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<HostFsProbe> {
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

impl HostFsTransportService {
    pub(crate) fn handle_interrupt(&self) {
        self.inner.device.handle_interrupt();
    }

    async fn raw_request(&self, bytes: Vec<u8>, response_len: usize) -> Result<Vec<u8>, IoError> {
        let (completion, rx) = oneshot::channel();
        self.inner
            .requests
            .push(Request::Raw {
                bytes,
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
        loop {
            let request = self.next_request().await;
            match request {
                Request::Raw {
                    bytes,
                    response_len,
                    completion,
                } => {
                    let mut response = vec![0_u8; response_len];
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
    type RequestFuture<'a>
        =
        core::pin::Pin<Box<dyn core::future::Future<Output = Result<Vec<u8>, IoError>> + Send + 'a>>
    where
        Self: 'a;

    fn mount_tag(&self) -> &str {
        self.inner.device.mount_tag()
    }

    fn request(&self, bytes: Vec<u8>, response_len: usize) -> Self::RequestFuture<'_> {
        let transport = self.clone();
        Box::pin(async move { transport.raw_request(bytes, response_len).await })
    }
}

fn discover_9p_device(
    fdt: &Fdt<'_>,
) -> Option<(Arc<helios_virtio::VirtioMmio9pDevice>, InterruptSourceId)> {
    for node in fdt.all_nodes() {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
        {
            continue;
        }

        let Some(region) = node.reg().and_then(|mut regs| regs.next()) else {
            continue;
        };
        let base = region.starting_address as usize;
        if !is_9p_mmio_device(base) {
            continue;
        }

        let header = core::ptr::NonNull::new(base as *mut u8)
            .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
        let mmio_size = region.size.unwrap();
        let irq_source = node
            .interrupts()
            .and_then(|mut interrupts| interrupts.next())
            .and_then(|irq| NonZeroU32::new(irq as u32))
            .map(InterruptSourceId)
            .unwrap_or_else(|| panic!("virtio-9p node at {base:#x} has no valid interrupt source"));
        let device =
            unsafe { helios_virtio::p9_from_mmio(header, mmio_size) }.unwrap_or_else(|error| {
                panic!("failed to initialize virtio-9p device at {base:#x}: {error}")
            });
        return Some((Arc::new(device), irq_source));
    }

    None
}

fn is_9p_mmio_device(base: usize) -> bool {
    crate::matches_virtio_mmio_device(base, helios_virtio::DeviceType::_9P)
}
